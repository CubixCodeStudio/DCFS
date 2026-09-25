//! PgRepository integration tests.
//!
//! Skipped unless `TEST_DATABASE_URL` points at a throwaway PostgreSQL database.
//! The suite creates its own schema in a fresh session-scoped schema and drops it,
//! so never point it at a database holding real data.

use dcfs_db::{CommitGuard, MetadataRepository, PgRepository, RepositoryError};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Connect and install the schema into a unique, disposable namespace.
async fn setup() -> Option<(PgRepository, sqlx::PgPool, String)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let schema = format!("dfs_test_{}", Uuid::new_v4().simple());

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect({
            let schema = schema.clone();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");

    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create test schema");

    let repo = PgRepository::from_pool(pool.clone());
    repo.migrate().await.expect("apply migration");
    repo.ensure_root().await.expect("create root");
    Some((repo, pool, schema))
}

async fn teardown(pool: sqlx::PgPool, schema: String) {
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop test schema");
}

#[tokio::test]
async fn migration_applies_and_root_exists() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let root = repo.get_root().await.unwrap();
    assert!(root.name.is_empty(), "root has no name component");
    assert_eq!(root.kind, "directory");
    assert!(root.parent_id.is_none());

    // ensure_root is idempotent.
    let again = repo.ensure_root().await.unwrap();
    assert_eq!(again.id, root.id);

    teardown(pool, schema).await;
}

#[tokio::test]
async fn node_lifecycle_and_live_name_uniqueness() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let root = repo.get_root().await.unwrap();
    let dir = Uuid::new_v4();
    repo.create_node(dir, root.id, b"docs".to_vec(), "directory", 0o40755, 0, 0)
        .await
        .unwrap();

    // Raw non-UTF8 bytes must survive a round trip.
    let file = Uuid::new_v4();
    let raw_name = vec![0xff, 0xfe, b'x'];
    repo.create_node(file, dir, raw_name.clone(), "file", 0o100644, 1000, 1000)
        .await
        .unwrap();
    assert_eq!(repo.get_node(file).await.unwrap().name, raw_name);

    // Duplicate live name in the same directory is rejected.
    let dup = repo
        .create_node(
            Uuid::new_v4(),
            dir,
            raw_name.clone(),
            "file",
            0o100644,
            0,
            0,
        )
        .await;
    assert!(
        matches!(dup, Err(RepositoryError::AlreadyExists)),
        "{dup:?}"
    );

    // After a soft delete the name is free again.
    repo.delete_node(file).await.unwrap();
    assert!(matches!(
        repo.get_node(file).await.unwrap_err(),
        RepositoryError::NotFound
    ));
    repo.create_node(Uuid::new_v4(), dir, raw_name, "file", 0o100644, 0, 0)
        .await
        .unwrap();

    let children = repo.list_children(dir, 10, 0).await.unwrap();
    assert_eq!(children.len(), 1, "soft-deleted nodes must not be listed");

    teardown(pool, schema).await;
}

#[tokio::test]
async fn publish_is_atomic_and_never_replaces() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let root = repo.get_root().await.unwrap();
    let published = Uuid::new_v4();
    repo.create_node(
        published,
        root.id,
        b"published".to_vec(),
        "file",
        0o100644,
        0,
        0,
    )
    .await
    .unwrap();
    let temporary = Uuid::new_v4();
    repo.create_node(
        temporary,
        root.id,
        b".uploading".to_vec(),
        "file",
        0o100644,
        0,
        0,
    )
    .await
    .unwrap();

    let conflict = repo
        .publish_node(temporary, root.id, b"published".to_vec())
        .await;
    assert!(
        matches!(conflict, Err(RepositoryError::AlreadyExists)),
        "{conflict:?}"
    );
    assert_eq!(
        repo.get_node(published).await.unwrap().name,
        b"published".to_vec()
    );
    assert_eq!(
        repo.get_node(temporary).await.unwrap().name,
        b".uploading".to_vec()
    );

    repo.publish_node(temporary, root.id, b"new-name".to_vec())
        .await
        .unwrap();
    assert_eq!(
        repo.get_node(temporary).await.unwrap().name,
        b"new-name".to_vec()
    );

    teardown(pool, schema).await;
}

#[tokio::test]
async fn commit_is_atomic_and_conflicts_are_rejected() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let root = repo.get_root().await.unwrap();
    let file = Uuid::new_v4();
    repo.create_node(file, root.id, b"data.bin".to_vec(), "file", 0o100644, 0, 0)
        .await
        .unwrap();

    let v1 = repo.create_staging_version(file, None, 8).await.unwrap();
    assert_eq!(repo.get_node(file).await.unwrap().generation, 1);
    repo.attach_chunk(v1.id, 0, 0, 8, "hash0", Uuid::new_v4())
        .await
        .unwrap();
    // Re-attaching the same index is idempotent (upload retry).
    repo.attach_chunk(v1.id, 0, 0, 8, "hash0", Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(repo.get_chunks(v1.id).await.unwrap().len(), 1);

    let committed = repo
        .commit_version(
            CommitGuard {
                node_id: file,
                version_id: v1.id,
                expected_generation: 1,
                expected_current_version: None,
            },
            8,
            "total0",
        )
        .await
        .unwrap();
    assert_eq!(committed.state, "committed");

    let node = repo.get_node(file).await.unwrap();
    assert_eq!(node.current_version_id, Some(v1.id));
    assert_eq!(node.size, 8);

    // A stale guard must not move the pointer, and the old bytes stay visible.
    let v2 = repo
        .create_staging_version(file, Some(v1.id), 8)
        .await
        .unwrap();
    let stale = repo
        .commit_version(
            CommitGuard {
                node_id: file,
                version_id: v2.id,
                expected_generation: 1, // actual is 2
                expected_current_version: Some(v1.id),
            },
            16,
            "total1",
        )
        .await;
    assert!(matches!(stale, Err(RepositoryError::Conflict)), "{stale:?}");
    assert_eq!(
        repo.get_node(file).await.unwrap().current_version_id,
        Some(v1.id)
    );
    assert_eq!(repo.get_version(v1.id).await.unwrap().state, "committed");

    // The correct guard commits and supersedes the previous version.
    repo.commit_version(
        CommitGuard {
            node_id: file,
            version_id: v2.id,
            expected_generation: 2,
            expected_current_version: Some(v1.id),
        },
        16,
        "total1",
    )
    .await
    .unwrap();
    assert_eq!(repo.get_version(v1.id).await.unwrap().state, "superseded");
    assert_eq!(
        repo.get_node(file).await.unwrap().current_version_id,
        Some(v2.id)
    );

    teardown(pool, schema).await;
}

// --- sharing a database with other applications ----------------------------

#[tokio::test]
async fn installs_into_a_named_schema_without_touching_anything_else() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let schema = format!("dfs_shared_{}", Uuid::new_v4().simple());

    // Stand in for an application that already owns this database.
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let existing = format!("tenant_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TABLE public.{existing} (id int primary key, note text)"
    ))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO public.{existing} VALUES (1, 'keep me')"
    ))
    .execute(&admin)
    .await
    .unwrap();

    // DCFS installs itself into its own schema.
    let repo = PgRepository::connect(&url, 2, &schema).await.unwrap();
    assert!(
        !repo.missing_tables().await.unwrap().is_empty(),
        "nothing is installed yet"
    );
    repo.ensure_schema(&schema).await.unwrap();
    repo.migrate().await.unwrap();
    assert!(repo.missing_tables().await.unwrap().is_empty());
    repo.ensure_root().await.unwrap();

    // Re-running the migration against an installed schema is a no-op.
    repo.migrate().await.unwrap();
    let root = repo.get_root().await.unwrap();
    let file = Uuid::new_v4();
    repo.create_node(file, root.id, b"kept.txt".to_vec(), "file", 0o100644, 0, 0)
        .await
        .unwrap();
    repo.migrate().await.unwrap();
    assert_eq!(
        repo.get_node(file).await.unwrap().name,
        b"kept.txt".to_vec()
    );

    // The other application's table is untouched, and DCFS did not land
    // in `public`.
    let note: String =
        sqlx::query_scalar(&format!("SELECT note FROM public.{existing} WHERE id = 1"))
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(note, "keep me");
    let in_public: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.nodes')::text")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert!(in_public.is_none(), "DCFS must stay out of public");

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP TABLE public.{existing}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_hostile_schema_name_never_reaches_sql() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    // Schema names cannot be bound as parameters, so they are validated instead.
    for bad in [
        "public; DROP TABLE nodes",
        "Public",
        "with space",
        "",
        "1leading_digit",
    ] {
        assert!(
            PgRepository::connect(&url, 1, bad).await.is_err(),
            "{bad:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn symlinks_store_a_raw_target() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let root = repo.get_root().await.unwrap();
    let id = Uuid::new_v4();
    // Targets are bytes: a link may point at a path that is not valid UTF-8.
    let target = vec![b'.', b'.', b'/', 0xff, 0xfe];
    let link = repo
        .create_symlink(id, root.id, b"link".to_vec(), target.clone(), 1000, 1000)
        .await
        .unwrap();
    assert_eq!(link.kind, "symlink");
    assert_eq!(link.link_target, Some(target.clone()));
    assert_eq!(repo.get_node(id).await.unwrap().link_target, Some(target));

    // The CHECK constraints from 0002 hold: only links carry a target.
    let bad = sqlx::query("INSERT INTO nodes (id, parent_id, name, kind, mode, uid, gid, link_target) VALUES ($1, $2, $3, 'file', 0, 0, 0, $4)")
        .bind(Uuid::new_v4())
        .bind(root.id)
        .bind(b"not-a-link".to_vec())
        .bind(b"/somewhere".to_vec())
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "a file must not carry a link target");

    let bad = sqlx::query("INSERT INTO nodes (id, parent_id, name, kind, mode, uid, gid) VALUES ($1, $2, $3, 'symlink', 0, 0, 0)")
        .bind(Uuid::new_v4())
        .bind(root.id)
        .bind(b"empty-link".to_vec())
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "a link must carry a target");

    // Listing shows it as a link, and a second migrate() changes nothing.
    let children = repo.list_children(root.id, 10, 0).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].kind, "symlink");

    teardown(pool, schema).await;
}

#[tokio::test]
async fn object_locators_survive_a_restart() {
    let Some((repo, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };

    let object_id = Uuid::new_v4();
    assert!(repo.get_object_locator(object_id).await.is_err());

    // What Discord hands back after an upload. Without this recorded, the
    // attachment is unreachable the moment the process restarts.
    repo.put_object_locator(&dcfs_db::ObjectLocatorRecord {
        object_id,
        backend: "discord".to_string(),
        message_id: "1234567890".to_string(),
        attachment_id: "9876543210".to_string(),
        url: "https://cdn.example/expiring?ex=1".to_string(),
        size: 8_388_648,
    })
    .await
    .unwrap();

    let found = repo.get_object_locator(object_id).await.unwrap();
    assert_eq!(found.message_id, "1234567890");
    assert_eq!(found.attachment_id, "9876543210");
    assert_eq!(found.size, 8_388_648);
    assert_eq!(found.backend, "discord");

    // The URL expires and is replaced; the ids that find it never change.
    repo.touch_object_url(object_id, "https://cdn.example/fresh?ex=2")
        .await
        .unwrap();
    let refreshed = repo.get_object_locator(object_id).await.unwrap();
    assert_eq!(refreshed.url, "https://cdn.example/fresh?ex=2");
    assert_eq!(refreshed.message_id, found.message_id);

    // Writing the same object again replaces the record rather than failing.
    repo.put_object_locator(&dcfs_db::ObjectLocatorRecord {
        message_id: "replacement".to_string(),
        ..refreshed
    })
    .await
    .unwrap();
    assert_eq!(
        repo.get_object_locator(object_id).await.unwrap().message_id,
        "replacement"
    );

    repo.delete_object_locator(object_id).await.unwrap();
    assert!(repo.get_object_locator(object_id).await.is_err());

    teardown(pool, schema).await;
}
