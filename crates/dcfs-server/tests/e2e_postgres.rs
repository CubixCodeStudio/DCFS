//! End-to-end test: the real router over a real PostgreSQL database.
//!
//! Skipped unless `TEST_DATABASE_URL` points at a **throwaway** database. Each
//! test creates its own schema and drops it afterwards, so never point this at
//! a database holding real data.
//!
//! ```bash
//! TEST_DATABASE_URL=postgresql://postgres:dev@localhost:55432/postgres \
//!   cargo test -p dcfs-server --test e2e_postgres
//! ```

mod common;

use axum::http::StatusCode;
use common::{key, name, send, send_bytes};
use dcfs_db::{MetadataRepository, PgRepository};
use dcfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use dcfs_server::{build_router, gc, AppState, TEST_CHUNK_SIZE};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// A router wired to a disposable PostgreSQL schema, plus the pool for direct
/// SQL assertions and the schema name for cleanup.
async fn setup() -> Option<(axum::Router, sqlx::PgPool, String)> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let schema = format!("dfs_e2e_{}", Uuid::new_v4().simple());

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

    let repo: Arc<dyn MetadataRepository> = Arc::new(repo);
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    let state = AppState::new(repo, store, [0x42; 32], TEST_CHUNK_SIZE);
    Some((build_router(state), pool, schema))
}

async fn teardown(pool: sqlx::PgPool, schema: String) {
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop test schema");
}

/// Upload one chunk: metadata in the query string, plaintext in the body.
async fn upload(router: &axum::Router, vid: &str, index: u64, data: &[u8]) -> StatusCode {
    let uri = format!(
        "/api/v1/versions/{vid}/chunks?chunk_index={index}&plaintext_size={}&plaintext_hash=&idempotency_key={}",
        data.len(),
        key()
    );
    send_bytes(router, "POST", &uri, data.to_vec()).await.0
}

macro_rules! pg_or_skip {
    () => {
        match setup().await {
            Some(parts) => parts,
            None => {
                eprintln!("skipped: TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn writes_a_multi_chunk_file_end_to_end() {
    let (router, pool, schema) = pg_or_skip!();

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    // mkdir /docs
    let (status, dir) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root_id, "name": name(b"docs"), "kind": "Directory",
            "mode": 0o40755, "uid": 1000, "gid": 1000, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dir}");
    let dir_id = dir["id"].as_str().unwrap().to_string();

    // touch /docs/<non-utf8 name>
    let raw = [0xff, 0xfe, b'x'];
    let (status, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": dir_id, "name": name(&raw), "kind": "File",
            "mode": 0o100644, "uid": 1000, "gid": 1000, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    let file_id = file["id"].as_str().unwrap().to_string();

    // stage + upload three chunks + commit
    let (_, staged) = send(
        &router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": file_id, "expected_generation": 0,
            "expected_current_version": null, "idempotency_key": key(),
        })),
    )
    .await;
    let vid = staged["version_id"].as_str().unwrap().to_string();

    for index in 0..3u64 {
        let status = upload(&router, &vid, index, &[index as u8; 8]).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, committed) = send(
        &router,
        "POST",
        &format!("/api/v1/versions/{vid}/commit"),
        Some(json!({
            "version_id": vid, "total_size": 24, "plaintext_hash": "total",
            "expected_generation": 1, "expected_current_version": null,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{committed}");

    // The API reports the committed state...
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file_id}"), None).await;
    assert_eq!(node["current_version_id"], vid);
    assert_eq!(node["size"], 24);
    assert_eq!(node["name"], name(&raw));

    // ...and so does the database, including the raw filename bytes.
    let row = sqlx::query("SELECT name, size, current_version_id FROM nodes WHERE id = $1")
        .bind(Uuid::parse_str(&file_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<Vec<u8>, _>("name"), raw.to_vec());
    assert_eq!(row.get::<i64, _>("size"), 24);
    assert_eq!(
        row.get::<Option<Uuid>, _>("current_version_id"),
        Some(Uuid::parse_str(&vid).unwrap())
    );

    let chunk_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks WHERE version_id = $1")
            .bind(Uuid::parse_str(&vid).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(chunk_count, 3);

    teardown(pool, schema).await;
}

#[tokio::test]
async fn a_losing_writer_conflicts_and_the_committed_version_survives() {
    let (router, pool, schema) = pg_or_skip!();

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();
    let (_, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root_id, "name": name(b"contended"), "kind": "File",
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        })),
    )
    .await;
    let file_id = file["id"].as_str().unwrap().to_string();

    // Both writers stage against generation 0 / no current version.
    let (_, a) = send(
        &router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": file_id, "expected_generation": 0,
            "expected_current_version": null, "idempotency_key": key(),
        })),
    )
    .await;
    let (_, b) = send(
        &router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": file_id, "expected_generation": 0,
            "expected_current_version": null, "idempotency_key": key(),
        })),
    )
    .await;
    let va = a["version_id"].as_str().unwrap().to_string();
    let vb = b["version_id"].as_str().unwrap().to_string();
    upload(&router, &va, 0, &[b'a'; 10]).await;
    upload(&router, &vb, 0, &[b'b'; 30]).await;

    // Staging twice bumped the generation twice, so only a guard matching the
    // current state (generation 2) can win.
    let (status, _) = send(
        &router,
        "POST",
        &format!("/api/v1/versions/{va}/commit"),
        Some(json!({
            "version_id": va, "total_size": 10, "plaintext_hash": "a",
            "expected_generation": 2, "expected_current_version": null,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/v1/versions/{vb}/commit"),
        Some(json!({
            "version_id": vb, "total_size": 30, "plaintext_hash": "b",
            "expected_generation": 2, "expected_current_version": null,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file_id}"), None).await;
    assert_eq!(
        node["current_version_id"], va,
        "the winner's bytes stay visible"
    );
    assert_eq!(node["size"], 10);

    // The loser's version is still staging; nothing was half-committed.
    let state: String = sqlx::query_scalar("SELECT state FROM file_versions WHERE id = $1")
        .bind(Uuid::parse_str(&vb).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "staging");

    teardown(pool, schema).await;
}

#[tokio::test]
async fn unlink_frees_the_name_and_hides_the_node() {
    let (router, pool, schema) = pg_or_skip!();

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    let make = |name_bytes: &'static [u8]| {
        let root_id = root_id.clone();
        let router = router.clone();
        async move {
            send(
                &router,
                "POST",
                "/api/v1/nodes",
                Some(json!({
                    "parent_id": root_id, "name": name(name_bytes), "kind": "File",
                    "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
                })),
            )
            .await
        }
    };

    let (status, first) = make(b"reused").await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = first["id"].as_str().unwrap().to_string();

    let (status, _) = make(b"reused").await;
    assert_eq!(status, StatusCode::CONFLICT, "live names are unique");

    let (status, _) = send(
        &router,
        "DELETE",
        &format!("/api/v1/nodes/{first_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = make(b"reused").await;
    assert_eq!(status, StatusCode::CREATED, "the name is free after unlink");

    // The deleted row is soft-deleted, not dropped, so GC can still reach it.
    let deleted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE deleted_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deleted, 1);

    let (_, listing) = send(
        &router,
        "GET",
        &format!("/api/v1/nodes/{root_id}/children"),
        None,
    )
    .await;
    assert_eq!(listing["children"].as_array().unwrap().len(), 1);

    teardown(pool, schema).await;
}

/// Two servers over one database, writing the same file. The in-process mutex
/// does nothing across instances, so this is entirely on the lock the database
/// hands out: without it both would open a staging version for the same file
/// and one writer's parts would end up in a version nobody commits.
#[tokio::test]
async fn two_servers_writing_one_file_share_a_single_write() {
    let Some((_, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL is not set");
        return;
    };

    // Two servers: their own state and their own in-process locks, over one
    // database and one object store, as a pair of instances behind a load
    // balancer would be.
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    let server = |store: Arc<dyn ObjectStore>| {
        let repo: Arc<dyn MetadataRepository> = Arc::new(PgRepository::from_pool(pool.clone()));
        build_router(AppState::new(repo, store, [0x42; 32], TEST_CHUNK_SIZE))
    };
    let router_a = server(store.clone());
    let router_b = server(store.clone());

    let (_, root) = send(&router_a, "GET", "/api/v1/nodes/root", None).await;
    let (_, file) = send(
        &router_a,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root["id"], "name": name(b"shared.bin"), "kind": "File",
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        })),
    )
    .await;
    let file_id = file["id"].as_str().unwrap().to_string();

    // Each server writes its own part of the file, at the same time.
    let a = vec![b'a'; TEST_CHUNK_SIZE as usize];
    let b = vec![b'b'; TEST_CHUNK_SIZE as usize];
    let at_zero = format!("/api/v1/nodes/{file_id}/data?offset=0");
    let at_part_one = format!("/api/v1/nodes/{file_id}/data?offset={TEST_CHUNK_SIZE}");
    let (first, second) = tokio::join!(
        send_bytes(&router_a, "PUT", &at_zero, a.clone()),
        send_bytes(&router_b, "PUT", &at_part_one, b.clone()),
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);

    // One write was open, not two: a second would have orphaned the first.
    let staging: i64 = sqlx::query(
        "SELECT count(*) FROM file_versions WHERE node_id = $1::uuid AND state = 'staging'",
    )
    .bind(&file_id)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(staging, 1, "both servers added to the same open write");

    // Either server can close it, and both parts are there.
    let (status, _) = send_bytes(
        &router_b,
        "POST",
        &format!("/api/v1/nodes/{file_id}/sync"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send_bytes(
        &router_a,
        "GET",
        &format!("/api/v1/nodes/{file_id}/data"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), (TEST_CHUNK_SIZE * 2) as usize);
    assert_eq!(&body[..TEST_CHUNK_SIZE as usize], &a[..]);
    assert_eq!(&body[TEST_CHUNK_SIZE as usize..], &b[..]);

    teardown(pool, schema).await;
}

/// Only one instance sweeps at a time. Two would read the same batch and race
/// each other to delete it — the same end state, reached twice, with the loser
/// logging failures for work already done.
#[tokio::test]
async fn a_second_instance_does_not_sweep_while_one_is() {
    let Some((_, pool, schema)) = setup().await else {
        eprintln!("skipped: TEST_DATABASE_URL is not set");
        return;
    };

    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    let repo_a: Arc<dyn MetadataRepository> = Arc::new(PgRepository::from_pool(pool.clone()));
    let repo_b: Arc<dyn MetadataRepository> = Arc::new(PgRepository::from_pool(pool.clone()));

    let sweeping = repo_a
        .try_lock_gc()
        .await
        .expect("ask for the sweeper's lock")
        .expect("nobody else holds it");

    assert!(
        repo_b.try_lock_gc().await.unwrap().is_none(),
        "the second instance is turned away rather than made to wait"
    );

    // Its tick does nothing and, importantly, is not an error: a sweep that is
    // already running covers this one.
    let skipped = gc::collect_once(&repo_b, &store, Duration::ZERO)
        .await
        .expect("a skipped sweep is not a failure");
    assert_eq!(skipped, Default::default());

    // The lock goes back when the sweeper is done, including when it ended
    // badly and never released anything itself.
    drop(sweeping);
    let mut regained = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if repo_b.try_lock_gc().await.unwrap().is_some() {
            regained = true;
            break;
        }
    }
    assert!(
        regained,
        "the next instance can sweep once the first is finished"
    );

    teardown(pool, schema).await;
}
