//! PostgreSQL implementation of [`MetadataRepository`].
//!
//! All queries are parameterized. `commit_version` is the only path allowed to
//! switch `nodes.current_version_id`, and it does so inside a transaction that
//! locks the node row.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    advisory_key, CommitGuard, FileChunkRecord, FileVersionRecord, MetadataRepository, NodeGuard,
    NodeRecord, ObjectLocatorRecord, RepositoryError, SessionRecord, GC_LOCK, MIGRATIONS,
};

/// PostgreSQL-backed metadata repository.
pub struct PgRepository {
    pool: PgPool,
}

/// The tables DCFS needs. Used to tell "not installed yet" apart from
/// "installed", so the server can say which without guessing.
const REQUIRED_TABLES: [&str; 5] = [
    "nodes",
    "file_versions",
    "file_chunks",
    "stored_objects",
    "sessions",
];

/// A PostgreSQL identifier we are about to interpolate into DDL.
///
/// Schema names cannot be bound as parameters, so this is the one place a
/// caller-supplied string reaches SQL text. Anything outside the unquoted
/// identifier grammar is rejected rather than escaped.
fn validate_schema_name(schema: &str) -> Result<(), RepositoryError> {
    let valid = !schema.is_empty()
        && schema.len() <= 63
        && schema.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && schema
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(RepositoryError::Database(format!(
            "invalid schema name {schema:?}: use lowercase letters, digits and underscores"
        )));
    }
    Ok(())
}

impl PgRepository {
    /// Connect with a bounded pool, pinned to one schema.
    ///
    /// Every pooled connection gets `search_path` set to `schema`, so DCFS
    /// can live in a database that already holds other applications' tables
    /// without colliding with them. This neither creates the schema nor
    /// installs tables: see [`PgRepository::migrate`] and
    /// [`PgRepository::missing_tables`].
    pub async fn connect(
        database_url: &str,
        max_connections: u32,
        schema: &str,
    ) -> Result<Self, RepositoryError> {
        validate_schema_name(schema)?;
        let schema = schema.to_string();
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .after_connect(move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    // pg_catalog stays on the path so built-in functions resolve.
                    sqlx::query(&format!("SET search_path TO {schema}, pg_catalog"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(database_url)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool (tests, shared pools).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Which of the required tables are not reachable on the current
    /// `search_path`. Empty means the schema is installed.
    pub async fn missing_tables(&self) -> Result<Vec<&'static str>, RepositoryError> {
        let mut missing = Vec::new();
        for table in REQUIRED_TABLES {
            // to_regclass resolves through search_path and returns NULL rather
            // than erroring when the table does not exist.
            let found: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
                .bind(table)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
            if found.is_none() {
                missing.push(table);
            }
        }
        Ok(missing)
    }

    /// Create `schema` if it does not exist. Additive: an existing schema and
    /// everything in it is left alone.
    pub async fn ensure_schema(&self, schema: &str) -> Result<(), RepositoryError> {
        validate_schema_name(schema)?;
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Apply the schema into the current `search_path`.
    ///
    /// The migration is additive and idempotent — every statement is
    /// `IF NOT EXISTS`, and it contains no DROP, ALTER, TRUNCATE or DELETE — so
    /// running it against a database that already has the tables, or that holds
    /// unrelated tables, changes nothing else. It is still a schema change, so
    /// the server only runs it when explicitly told to.
    pub async fn migrate(&self) -> Result<(), RepositoryError> {
        for migration in MIGRATIONS {
            sqlx::raw_sql(migration)
                .execute(&self.pool)
                .await
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        Ok(())
    }

    /// Insert the root directory if the namespace is empty.
    pub async fn ensure_root(&self) -> Result<NodeRecord, RepositoryError> {
        if let Some(row) = sqlx::query(ROOT_SELECT)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
        {
            return Ok(node_from_row(&row));
        }

        // Racing servers may both try to insert; ON CONFLICT DO NOTHING then re-read.
        sqlx::query(
            "INSERT INTO nodes (id, parent_id, name, kind, mode, uid, gid)
             VALUES ($1, NULL, $2, 'directory', $3, 0, 0)
             ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(Vec::<u8>::new()) // root has no name component
        .bind(0o40755_i32)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        let row = sqlx::query(ROOT_SELECT)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .ok_or(RepositoryError::NotFound)?;
        Ok(node_from_row(&row))
    }
}

const NODE_COLUMNS: &str = "id, parent_id, name, kind, mode, uid, gid, size, atime, mtime, ctime, current_version_id, generation, link_target";

const ROOT_SELECT: &str = "SELECT id, parent_id, name, kind, mode, uid, gid, size, atime, mtime, ctime, current_version_id, generation, link_target
     FROM nodes WHERE parent_id IS NULL AND deleted_at IS NULL LIMIT 1";

const VERSION_COLUMNS: &str =
    "id, node_id, base_version_id, state, size, plaintext_hash, chunk_size, created_at, committed_at";

fn db_err(e: sqlx::Error) -> RepositoryError {
    match &e {
        sqlx::Error::RowNotFound => RepositoryError::NotFound,
        sqlx::Error::Database(dbe) if dbe.code().as_deref() == Some("23505") => {
            RepositoryError::AlreadyExists
        }
        _ => RepositoryError::Database(e.to_string()),
    }
}

fn node_from_row(row: &PgRow) -> NodeRecord {
    NodeRecord {
        id: row.get("id"),
        parent_id: row.get("parent_id"),
        name: row.get("name"),
        kind: row.get("kind"),
        mode: row.get("mode"),
        uid: row.get("uid"),
        gid: row.get("gid"),
        size: row.get("size"),
        atime: row.get("atime"),
        mtime: row.get("mtime"),
        ctime: row.get("ctime"),
        current_version_id: row.get("current_version_id"),
        generation: row.get("generation"),
        link_target: row.get("link_target"),
    }
}

fn chunk_from_row(row: &PgRow) -> FileChunkRecord {
    FileChunkRecord {
        version_id: row.get("version_id"),
        chunk_index: row.get("chunk_index"),
        logical_offset: row.get("logical_offset"),
        plaintext_size: row.get("plaintext_size"),
        plaintext_hash: row.get("plaintext_hash"),
        object_id: row.get("object_id"),
    }
}

fn version_from_row(row: &PgRow) -> FileVersionRecord {
    FileVersionRecord {
        id: row.get("id"),
        node_id: row.get("node_id"),
        base_version_id: row.get("base_version_id"),
        state: row.get("state"),
        size: row.get("size"),
        plaintext_hash: row.get("plaintext_hash"),
        chunk_size: row.get("chunk_size"),
        created_at: row.get("created_at"),
        committed_at: row.get("committed_at"),
    }
}

#[async_trait]
impl MetadataRepository for PgRepository {
    async fn get_node(&self, id: Uuid) -> Result<NodeRecord, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(node_from_row(&row))
    }

    async fn get_root(&self) -> Result<NodeRecord, RepositoryError> {
        let row = sqlx::query(ROOT_SELECT)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .ok_or(RepositoryError::NotFound)?;
        Ok(node_from_row(&row))
    }

    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NodeRecord>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes
             WHERE parent_id = $1 AND deleted_at IS NULL
             ORDER BY name LIMIT $2 OFFSET $3"
        ))
        .bind(parent_id)
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(node_from_row).collect())
    }

    async fn find_child(
        &self,
        parent_id: Uuid,
        name: &[u8],
    ) -> Result<NodeRecord, RepositoryError> {
        // Served by idx_nodes_live_name, the same partial unique index that
        // keeps names unique within a directory.
        let row = sqlx::query(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes
             WHERE parent_id = $1 AND name = $2 AND deleted_at IS NULL"
        ))
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(node_from_row(&row))
    }

    async fn create_node(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        kind: &str,
        mode: i32,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError> {
        let row = sqlx::query(&format!(
            "INSERT INTO nodes (id, parent_id, name, kind, mode, uid, gid)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {NODE_COLUMNS}"
        ))
        .bind(id)
        .bind(parent_id)
        .bind(name)
        .bind(kind)
        .bind(mode)
        .bind(uid)
        .bind(gid)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(node_from_row(&row))
    }

    async fn create_symlink(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        target: Vec<u8>,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError> {
        let row = sqlx::query(&format!(
            "INSERT INTO nodes (id, parent_id, name, kind, mode, uid, gid, size, link_target)
             VALUES ($1, $2, $3, 'symlink', $4, $5, $6, $7, $8)
             RETURNING {NODE_COLUMNS}"
        ))
        .bind(id)
        .bind(parent_id)
        .bind(name)
        // The mode of a symlink is conventionally 0o120777: the link itself
        // carries no permissions, the target's are what count.
        .bind(0o120777_i32)
        .bind(uid)
        .bind(gid)
        .bind(target.len() as i64)
        .bind(target)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(node_from_row(&row))
    }

    async fn delete_node(&self, id: Uuid) -> Result<(), RepositoryError> {
        // Soft delete: the partial unique index frees the name, GC reclaims objects later.
        let done =
            sqlx::query("UPDATE nodes SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        if done.rows_affected() == 0 {
            return Err(RepositoryError::NotFound);
        }
        Ok(())
    }

    async fn rename_node(
        &self,
        id: Uuid,
        new_parent_id: Uuid,
        new_name: Vec<u8>,
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Whatever sits at the destination is replaced, as rename(2) requires.
        // FOR UPDATE keeps a concurrent writer from creating a second entry
        // under the same name between the check and the update.
        let existing = sqlx::query(
            "SELECT id, kind FROM nodes
             WHERE parent_id = $1 AND name = $2 AND deleted_at IS NULL
             FOR UPDATE",
        )
        .bind(new_parent_id)
        .bind(&new_name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;

        if let Some(row) = existing {
            let victim: Uuid = row.get("id");
            if victim != id {
                if row.get::<String, _>("kind") == "directory" {
                    let child: Option<Uuid> = sqlx::query_scalar(
                        "SELECT id FROM nodes
                         WHERE parent_id = $1 AND deleted_at IS NULL LIMIT 1",
                    )
                    .bind(victim)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    if child.is_some() {
                        return Err(RepositoryError::DirectoryNotEmpty);
                    }
                }
                // Soft delete, so the partial unique index frees the name and
                // GC can still reclaim the replaced file's objects.
                sqlx::query("UPDATE nodes SET deleted_at = NOW() WHERE id = $1")
                    .bind(victim)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
        }

        let done = sqlx::query(
            "UPDATE nodes SET parent_id = $2, name = $3, ctime = NOW()
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(new_parent_id)
        .bind(new_name)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if done.rows_affected() == 0 {
            return Err(RepositoryError::NotFound);
        }

        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn update_node_attr(
        &self,
        id: Uuid,
        mode: Option<i32>,
        uid: Option<i32>,
        gid: Option<i32>,
        size: Option<i64>,
        mtime: Option<DateTime<Utc>>,
        atime: Option<DateTime<Utc>>,
    ) -> Result<NodeRecord, RepositoryError> {
        // COALESCE keeps this one statement instead of a read-modify-write race.
        let row = sqlx::query(&format!(
            "UPDATE nodes SET
                 mode  = COALESCE($2, mode),
                 uid   = COALESCE($3, uid),
                 gid   = COALESCE($4, gid),
                 size  = COALESCE($5, size),
                 mtime = COALESCE($6, mtime),
                 atime = COALESCE($7, atime),
                 ctime = NOW()
             WHERE id = $1 AND deleted_at IS NULL
             RETURNING {NODE_COLUMNS}"
        ))
        .bind(id)
        .bind(mode)
        .bind(uid)
        .bind(gid)
        .bind(size)
        .bind(mtime)
        .bind(atime)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(node_from_row(&row))
    }

    async fn create_staging_version(
        &self,
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        chunk_size: i64,
    ) -> Result<FileVersionRecord, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        let bumped = sqlx::query(
            "UPDATE nodes SET generation = generation + 1
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(node_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if bumped.rows_affected() == 0 {
            return Err(RepositoryError::NotFound);
        }

        let row = sqlx::query(&format!(
            "INSERT INTO file_versions (id, node_id, base_version_id, state, size, plaintext_hash, chunk_size)
             VALUES ($1, $2, $3, 'staging', 0, '', $4)
             RETURNING {VERSION_COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(node_id)
        .bind(base_version_id)
        .bind(chunk_size)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(version_from_row(&row))
    }

    async fn lock_node(&self, node_id: Uuid) -> Result<NodeGuard, RepositoryError> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        // Session-scoped, not transaction-scoped: the work it guards writes
        // objects to a remote store between queries, which has no business
        // inside a database transaction.
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(advisory_key(node_id))
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(NodeGuard::holding(conn))
    }

    async fn try_lock_gc(&self) -> Result<Option<NodeGuard>, RepositoryError> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(GC_LOCK.0)
            .bind(GC_LOCK.1)
            .fetch_one(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(taken.then(|| NodeGuard::holding(conn)))
    }

    async fn find_open_staging_version(
        &self,
        node_id: Uuid,
    ) -> Result<FileVersionRecord, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {VERSION_COLUMNS} FROM file_versions
             WHERE node_id = $1 AND state = 'staging'
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(version_from_row(&row))
    }

    async fn touch_staging_version(
        &self,
        version_id: Uuid,
        size: i64,
    ) -> Result<(), RepositoryError> {
        // created_at doubles as "last active" for a staging version: the
        // collector decides a version is abandoned by its age, and an upload
        // that is still going must not look old.
        sqlx::query(
            "UPDATE file_versions SET size = $2, created_at = NOW()
             WHERE id = $1 AND state = 'staging'",
        )
        .bind(version_id)
        .bind(size)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn reconcile_node_sizes(&self) -> Result<u64, RepositoryError> {
        let done = sqlx::query(
            "UPDATE nodes n
             SET size = COALESCE((SELECT v.size FROM file_versions v
                                  WHERE v.id = n.current_version_id), 0)
             WHERE n.kind = 'file'
               AND n.size <> COALESCE((SELECT v.size FROM file_versions v
                                       WHERE v.id = n.current_version_id), 0)
               AND NOT EXISTS (SELECT 1 FROM file_versions s
                               WHERE s.node_id = n.id AND s.state = 'staging')",
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn attach_chunk(
        &self,
        version_id: Uuid,
        chunk_index: i64,
        logical_offset: i64,
        plaintext_size: i32,
        plaintext_hash: &str,
        object_id: Uuid,
    ) -> Result<(), RepositoryError> {
        // Re-uploading the same chunk index must be idempotent (retries after a
        // partial failure), so the last writer for an index wins.
        sqlx::query(
            "INSERT INTO file_chunks
                 (version_id, chunk_index, logical_offset, plaintext_size, plaintext_hash, object_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (version_id, chunk_index) DO UPDATE SET
                 logical_offset = EXCLUDED.logical_offset,
                 plaintext_size = EXCLUDED.plaintext_size,
                 plaintext_hash = EXCLUDED.plaintext_hash,
                 object_id      = EXCLUDED.object_id",
        )
        .bind(version_id)
        .bind(chunk_index)
        .bind(logical_offset)
        .bind(plaintext_size)
        .bind(plaintext_hash)
        .bind(object_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_chunks(&self, version_id: Uuid) -> Result<Vec<FileChunkRecord>, RepositoryError> {
        // An unknown version is NotFound; a known version with no chunks is an empty list.
        let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM file_versions WHERE id = $1")
            .bind(version_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        if exists.is_none() {
            return Err(RepositoryError::NotFound);
        }

        let rows = sqlx::query(
            "SELECT version_id, chunk_index, logical_offset, plaintext_size, plaintext_hash, object_id
             FROM file_chunks WHERE version_id = $1 ORDER BY chunk_index",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(rows.iter().map(chunk_from_row).collect())
    }

    async fn get_chunk_range(
        &self,
        version_id: Uuid,
        first: i64,
        last: i64,
    ) -> Result<Vec<FileChunkRecord>, RepositoryError> {
        // The primary key is (version_id, chunk_index), so this is an index
        // range scan however large the file is.
        let rows = sqlx::query(
            "SELECT version_id, chunk_index, logical_offset, plaintext_size, plaintext_hash, object_id
             FROM file_chunks
             WHERE version_id = $1 AND chunk_index BETWEEN $2 AND $3
             ORDER BY chunk_index",
        )
        .bind(version_id)
        .bind(first)
        .bind(last)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(chunk_from_row).collect())
    }

    async fn copy_chunk_range(
        &self,
        from_version: Uuid,
        to_version: Uuid,
        first: i64,
        last: i64,
    ) -> Result<u64, RepositoryError> {
        if first > last {
            return Ok(0);
        }
        let done = sqlx::query(
            "INSERT INTO file_chunks
                 (version_id, chunk_index, logical_offset, plaintext_size, plaintext_hash, object_id)
             SELECT $2, chunk_index, logical_offset, plaintext_size, plaintext_hash, object_id
             FROM file_chunks
             WHERE version_id = $1 AND chunk_index BETWEEN $3 AND $4
             ON CONFLICT (version_id, chunk_index) DO NOTHING",
        )
        .bind(from_version)
        .bind(to_version)
        .bind(first)
        .bind(last)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn commit_version(
        &self,
        guard: CommitGuard,
        total_size: i64,
        plaintext_hash: &str,
    ) -> Result<FileVersionRecord, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Lock the node row first: concurrent committers serialize here, so the
        // generation/current-version check below cannot be read-then-stale.
        let node = sqlx::query(
            "SELECT current_version_id, generation FROM nodes
             WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(guard.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;

        let generation: i64 = node.get("generation");
        let current_version_id: Option<Uuid> = node.get("current_version_id");
        if generation != guard.expected_generation
            || current_version_id != guard.expected_current_version
        {
            // Dropping tx rolls back; the caller keeps its dirty cache.
            return Err(RepositoryError::Conflict);
        }

        let row = sqlx::query(&format!(
            "UPDATE file_versions
             SET state = 'committed', size = $2, plaintext_hash = $3, committed_at = NOW()
             WHERE id = $1 AND state = 'staging'
             RETURNING {VERSION_COLUMNS}"
        ))
        .bind(guard.version_id)
        .bind(total_size)
        .bind(plaintext_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;

        if let Some(old) = current_version_id {
            sqlx::query("UPDATE file_versions SET state = 'superseded' WHERE id = $1 AND state = 'committed'")
                .bind(old)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }

        sqlx::query(
            "UPDATE nodes SET current_version_id = $2, size = $3, mtime = NOW(), ctime = NOW()
             WHERE id = $1",
        )
        .bind(guard.node_id)
        .bind(guard.version_id)
        .bind(total_size)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(version_from_row(&row))
    }

    async fn put_object_locator(
        &self,
        locator: &ObjectLocatorRecord,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO stored_objects
                 (id, backend, message_id, attachment_id, url, ciphertext_size,
                  crypto_key_id, object_hash)
             VALUES ($1, $2, $3, $4, $5, $6, '', '')
             ON CONFLICT (id) DO UPDATE SET
                 backend = EXCLUDED.backend,
                 message_id = EXCLUDED.message_id,
                 attachment_id = EXCLUDED.attachment_id,
                 url = EXCLUDED.url,
                 ciphertext_size = EXCLUDED.ciphertext_size",
        )
        .bind(locator.object_id)
        .bind(&locator.backend)
        .bind(&locator.message_id)
        .bind(&locator.attachment_id)
        .bind(&locator.url)
        .bind(locator.size)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_object_locator(
        &self,
        object_id: Uuid,
    ) -> Result<ObjectLocatorRecord, RepositoryError> {
        let row = sqlx::query(
            "SELECT id, backend, message_id, attachment_id, url, ciphertext_size
             FROM stored_objects WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(object_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(ObjectLocatorRecord {
            object_id: row.get("id"),
            backend: row.get::<Option<String>, _>("backend").unwrap_or_default(),
            message_id: row.get("message_id"),
            attachment_id: row.get("attachment_id"),
            url: row.get::<Option<String>, _>("url").unwrap_or_default(),
            size: row.get("ciphertext_size"),
        })
    }

    async fn touch_object_url(&self, object_id: Uuid, url: &str) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE stored_objects SET url = $2 WHERE id = $1")
            .bind(object_id)
            .bind(url)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn delete_object_locator(&self, object_id: Uuid) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM stored_objects WHERE id = $1")
            .bind(object_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn create_session(
        &self,
        id: Uuid,
        token_hash: &str,
        label: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, RepositoryError> {
        let row = sqlx::query(
            "INSERT INTO sessions (id, user_id, namespace_id, token_hash, label, expires_at)
             VALUES ($1, $2, $3, $4, $2, $5)
             RETURNING id, label, created_at, expires_at",
        )
        .bind(id)
        .bind(label)
        .bind(Uuid::nil()) // v0.1 has exactly one namespace
        .bind(token_hash)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(SessionRecord {
            id: row.get("id"),
            label: row.get::<Option<String>, _>("label").unwrap_or_default(),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        })
    }

    async fn find_session(&self, token_hash: &str) -> Result<SessionRecord, RepositoryError> {
        let row = sqlx::query(
            "SELECT id, label, created_at, expires_at FROM sessions
             WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > NOW()",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(SessionRecord {
            id: row.get("id"),
            label: row.get::<Option<String>, _>("label").unwrap_or_default(),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
        })
    }

    async fn revoke_session(&self, id: Uuid) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE sessions SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn purge_expired_sessions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let done = sqlx::query("DELETE FROM sessions WHERE expires_at < $1")
            .bind(older_than)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn collectable_objects(
        &self,
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Uuid>, RepositoryError> {
        // `live` is the set of versions whose chunks must never be collected:
        // the current version of a node that still exists, or one still being
        // staged. A staging version can only reference objects it just created,
        // or objects carried over from the node's current version — which is
        // live — so a version staged while this runs cannot lose an object to
        // it.
        let rows: Vec<Uuid> = sqlx::query_scalar(
            "WITH live AS (
                 SELECT v.id
                 FROM file_versions v
                 JOIN nodes n ON n.id = v.node_id
                 WHERE n.deleted_at IS NULL AND n.current_version_id = v.id
                 UNION
                 -- A staging version is live only while its upload could still
                 -- be in progress. One abandoned by a client that died would
                 -- otherwise protect its chunks forever.
                 SELECT id FROM file_versions
                 WHERE state = 'staging' AND created_at >= $1
             )
             SELECT DISTINCT c.object_id
             FROM file_chunks c
             JOIN file_versions v ON v.id = c.version_id
             WHERE v.id NOT IN (SELECT id FROM live)
               AND COALESCE(v.committed_at, v.created_at) < $1
               AND NOT EXISTS (
                   SELECT 1
                   FROM file_chunks other
                   WHERE other.object_id = c.object_id
                     AND other.version_id IN (SELECT id FROM live)
               )
             LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn forget_object(&self, object_id: Uuid) -> Result<u64, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let chunks = sqlx::query("DELETE FROM file_chunks WHERE object_id = $1")
            .bind(object_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM stored_objects WHERE id = $1")
            .bind(object_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(chunks.rows_affected())
    }

    async fn purge_empty_dead_versions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError> {
        let done = sqlx::query(
            "DELETE FROM file_versions v
             WHERE COALESCE(v.committed_at, v.created_at) < $1
               AND NOT EXISTS (
                   SELECT 1 FROM nodes n
                   WHERE n.current_version_id = v.id AND n.deleted_at IS NULL
               )
               AND NOT EXISTS (SELECT 1 FROM file_chunks c WHERE c.version_id = v.id)",
        )
        .bind(older_than)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn purge_deleted_nodes(&self, older_than: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let done = sqlx::query(
            "DELETE FROM nodes n
             WHERE n.deleted_at IS NOT NULL
               AND n.deleted_at < $1
               AND NOT EXISTS (SELECT 1 FROM file_versions v WHERE v.node_id = n.id)
               AND NOT EXISTS (SELECT 1 FROM nodes c WHERE c.parent_id = n.id)",
        )
        .bind(older_than)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn get_version(&self, id: Uuid) -> Result<FileVersionRecord, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {VERSION_COLUMNS} FROM file_versions WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(RepositoryError::NotFound)?;
        Ok(version_from_row(&row))
    }
}
