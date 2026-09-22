//! Metadata repository traits.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use thiserror::Error;
use uuid::Uuid;

/// Repository error types.
#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("conflict")]
    Conflict,
    #[error("directory not empty")]
    DirectoryNotEmpty,
    #[error("database error: {0}")]
    Database(String),
}

/// Node record from database.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: Vec<u8>,
    pub kind: String,
    pub mode: i32,
    pub uid: i32,
    pub gid: i32,
    pub size: i64,
    pub atime: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
    pub ctime: DateTime<Utc>,
    pub current_version_id: Option<Uuid>,
    pub generation: i64,
    /// Raw target bytes, set only on symlinks.
    pub link_target: Option<Vec<u8>>,
}

/// File version record from database.
#[derive(Debug, Clone)]
pub struct FileVersionRecord {
    pub id: Uuid,
    pub node_id: Uuid,
    pub base_version_id: Option<Uuid>,
    pub state: String,
    pub size: i64,
    pub plaintext_hash: String,
    pub chunk_size: i64,
    pub created_at: DateTime<Utc>,
    pub committed_at: Option<DateTime<Utc>>,
}

/// File chunk record from database.
#[derive(Debug, Clone)]
pub struct FileChunkRecord {
    pub version_id: Uuid,
    pub chunk_index: i64,
    pub logical_offset: i64,
    pub plaintext_size: i32,
    pub plaintext_hash: String,
    pub object_id: Uuid,
}

/// Holds a node's write lock for as long as it lives.
///
/// The lock belongs to the connection that took it, so it is released here
/// rather than left for the connection to carry back into the pool. Dropping
/// is the only release: a write that fails partway never reaches a tidy
/// ending, and a lock that outlived one would stop every later writer.
pub struct NodeGuard {
    /// None for a store that is already confined to one process.
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

impl NodeGuard {
    /// A guard over a store that needs no lock beyond the process it runs in.
    pub fn unlocked() -> Self {
        Self { conn: None }
    }

    pub fn holding(conn: sqlx::pool::PoolConnection<sqlx::Postgres>) -> Self {
        Self { conn: Some(conn) }
    }
}

impl Drop for NodeGuard {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        // Dropping cannot wait, so the release goes to the runtime. Should it
        // never run — at shutdown, say — closing the connection ends its
        // session and the lock with it.
        tokio::spawn(async move {
            if let Err(e) = sqlx::query("SELECT pg_advisory_unlock_all()")
                .execute(&mut *conn)
                .await
            {
                tracing::warn!("releasing a node lock failed: {e}");
            }
        });
    }
}

/// Names the sweeper's lock.
///
/// The two-argument advisory locks are a different space from the
/// one-argument ones [`advisory_key`] uses, so this cannot collide with a
/// node's lock however the ids fall.
pub const GC_LOCK: (i32, i32) = (0x6466_7300, 1);

/// Fold a node id into the single integer an advisory lock is named by.
///
/// Two nodes can land on the same number, which costs them a little
/// serialisation against each other and nothing else — the lock is a
/// mutual-exclusion device, not an identity.
pub fn advisory_key(node_id: Uuid) -> i64 {
    // FNV-1a: stable across releases and architectures, which a hasher from
    // the standard library is not promised to be.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in node_id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as i64
}

/// A credential with an expiry, identified by the hash of its token.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: Uuid,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Where one stored object actually lives in the storage backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectLocatorRecord {
    pub object_id: Uuid,
    /// Which backend wrote it, so a deployment that changes backends can tell
    /// what it can still reach.
    pub backend: String,
    pub message_id: String,
    pub attachment_id: String,
    /// Last CDN URL seen. A cache: these expire and are refreshed on use.
    pub url: String,
    pub size: i64,
}

/// Guard for atomic version commit.
#[derive(Debug, Clone)]
pub struct CommitGuard {
    pub node_id: Uuid,
    pub version_id: Uuid,
    pub expected_generation: i64,
    pub expected_current_version: Option<Uuid>,
}

/// Metadata repository trait.
///
/// Several methods take one argument per column rather than a struct: the
/// columns are the contract, and a struct here would only be unpacked again on
/// both sides.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait MetadataRepository: Send + Sync + 'static {
    /// Get a node by ID.
    async fn get_node(&self, id: Uuid) -> Result<NodeRecord, RepositoryError>;

    /// Get root node.
    async fn get_root(&self) -> Result<NodeRecord, RepositoryError>;

    /// List children of a directory.
    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NodeRecord>, RepositoryError>;

    /// Find one child of a directory by its raw name.
    ///
    /// Path resolution asks this once per component. Listing a directory to
    /// find one entry makes every `open` cost as much as the directory is
    /// wide, which for a directory of ten thousand files is ten thousand rows
    /// per lookup.
    async fn find_child(&self, parent_id: Uuid, name: &[u8])
        -> Result<NodeRecord, RepositoryError>;

    /// Create a new node.
    async fn create_node(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        kind: &str,
        mode: i32,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError>;

    /// Create a symbolic link. Its target is raw bytes: a link may point at a
    /// path that is not valid UTF-8, or at nothing at all.
    async fn create_symlink(
        &self,
        id: Uuid,
        parent_id: Uuid,
        name: Vec<u8>,
        target: Vec<u8>,
        uid: i32,
        gid: i32,
    ) -> Result<NodeRecord, RepositoryError>;

    /// Delete a node.
    async fn delete_node(&self, id: Uuid) -> Result<(), RepositoryError>;

    /// Rename a node, replacing whatever already lives at the destination.
    ///
    /// POSIX `rename(2)` replaces the destination atomically, and the whole
    /// write-a-temp-file-then-rename idiom depends on it: git, editors and
    /// package managers all save that way. The replacement and the rename
    /// happen in one transaction so a crash cannot leave the destination
    /// deleted and the source un-renamed.
    ///
    /// Returns [`RepositoryError::DirectoryNotEmpty`] if the destination is a
    /// directory that still has children.
    async fn rename_node(
        &self,
        id: Uuid,
        new_parent_id: Uuid,
        new_name: Vec<u8>,
    ) -> Result<(), RepositoryError>;

    /// Update node attributes.
    async fn update_node_attr(
        &self,
        id: Uuid,
        mode: Option<i32>,
        uid: Option<i32>,
        gid: Option<i32>,
        size: Option<i64>,
        mtime: Option<DateTime<Utc>>,
        atime: Option<DateTime<Utc>>,
    ) -> Result<NodeRecord, RepositoryError>;

    /// Create a staging file version.
    async fn create_staging_version(
        &self,
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        chunk_size: i64,
    ) -> Result<FileVersionRecord, RepositoryError>;

    /// Hold the write lock for one node until the guard is dropped.
    ///
    /// Serialising writers inside one process is not enough once a second
    /// server is talking to the same database: two of them would each open a
    /// staging version for the same file and one would be orphaned. A lock the
    /// database hands out covers every instance.
    async fn lock_node(&self, node_id: Uuid) -> Result<NodeGuard, RepositoryError>;

    /// Take the sweeper's lock, or report that another instance holds it.
    ///
    /// Two instances sweeping at once is not corrupting — deleting an object
    /// twice is the same end state — but they read the same batch, race each
    /// other to delete it, and the loser logs failures for work that was
    /// already done. One sweeper at a time is simply what the job wants.
    ///
    /// Returns `None` rather than waiting: a sweep that is already running
    /// covers this tick, and blocking would only pile up ticks behind it.
    async fn try_lock_gc(&self) -> Result<Option<NodeGuard>, RepositoryError>;

    /// The newest staging version of a node, if a write is in progress.
    ///
    /// A sequential write used to create one version per request, and each new
    /// version copied the whole manifest of the one before it, so writing an
    /// N-part file wrote N(N+1)/2 chunk rows. Reusing one staging version for
    /// the whole write makes that N.
    async fn find_open_staging_version(
        &self,
        node_id: Uuid,
    ) -> Result<FileVersionRecord, RepositoryError>;

    /// Record how long the file is as a staging version grows, and keep the
    /// version from looking abandoned while its upload is still going.
    async fn touch_staging_version(
        &self,
        version_id: Uuid,
        size: i64,
    ) -> Result<(), RepositoryError>;

    /// Put a node's size back to what its committed version actually holds.
    ///
    /// A write in progress advances the node's size so that `stat` tells the
    /// truth while it runs. If that write is then abandoned, the size has to
    /// come back down, or the file reads as zero-filled past its real end.
    async fn reconcile_node_sizes(&self) -> Result<u64, RepositoryError>;

    /// Attach a chunk to a version.
    async fn attach_chunk(
        &self,
        version_id: Uuid,
        chunk_index: i64,
        logical_offset: i64,
        plaintext_size: i32,
        plaintext_hash: &str,
        object_id: Uuid,
    ) -> Result<(), RepositoryError>;

    /// Get chunks for a version.
    async fn get_chunks(&self, version_id: Uuid) -> Result<Vec<FileChunkRecord>, RepositoryError>;

    /// Get only the chunks in `first..=last`.
    ///
    /// A read touches a handful of chunks whatever the file's size, so fetching
    /// the whole manifest to pick a few out of it makes every read cost as much
    /// as the file is long.
    async fn get_chunk_range(
        &self,
        version_id: Uuid,
        first: i64,
        last: i64,
    ) -> Result<Vec<FileChunkRecord>, RepositoryError>;

    /// Copy chunks `first..=last` from one version's manifest to another's.
    ///
    /// A new version carries over every chunk the write did not touch. Copying
    /// them in one statement keeps an append to a large file from costing one
    /// round trip per existing chunk.
    async fn copy_chunk_range(
        &self,
        from_version: Uuid,
        to_version: Uuid,
        first: i64,
        last: i64,
    ) -> Result<u64, RepositoryError>;

    /// Atomically commit a version.
    async fn commit_version(
        &self,
        guard: CommitGuard,
        total_size: i64,
        plaintext_hash: &str,
    ) -> Result<FileVersionRecord, RepositoryError>;

    /// Get a version by ID.
    async fn get_version(&self, id: Uuid) -> Result<FileVersionRecord, RepositoryError>;

    // --- object locators ----------------------------------------------------
    //
    // An object in a remote backend is reachable only through whatever the
    // backend gave back when it was written. Keeping that in memory means an
    // object cannot be found again after a restart, so it lives here.

    /// Record where an object was stored, replacing any earlier record.
    async fn put_object_locator(
        &self,
        locator: &ObjectLocatorRecord,
    ) -> Result<(), RepositoryError>;

    /// Where an object was stored, if it is known.
    async fn get_object_locator(
        &self,
        object_id: Uuid,
    ) -> Result<ObjectLocatorRecord, RepositoryError>;

    /// Update the cached URL for an object after refreshing it.
    async fn touch_object_url(&self, object_id: Uuid, url: &str) -> Result<(), RepositoryError>;

    /// Forget where an object was stored, after the backend has deleted it.
    async fn delete_object_locator(&self, object_id: Uuid) -> Result<(), RepositoryError>;

    // --- sessions ----------------------------------------------------------

    /// Store a new session. `token_hash` is a hash of the token, never the
    /// token: a copy of the table must not be usable as a credential.
    async fn create_session(
        &self,
        id: Uuid,
        token_hash: &str,
        label: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, RepositoryError>;

    /// Look up a live session by its token hash. Expired and revoked sessions
    /// are not found.
    async fn find_session(&self, token_hash: &str) -> Result<SessionRecord, RepositoryError>;

    /// Revoke a session by id. Revoking one that is already gone is not an error.
    async fn revoke_session(&self, id: Uuid) -> Result<bool, RepositoryError>;

    /// Delete sessions that expired before `older_than`.
    async fn purge_expired_sessions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError>;

    // --- garbage collection ------------------------------------------------
    //
    // A version is *live* when it is the current version of a node that has not
    // been deleted, or when it is still staging. Everything else — superseded
    // versions, versions of a deleted node — is dead and its chunks can go.
    //
    // Objects are shared: an edit reuses the object ids of the chunks it did
    // not touch, so the same object can be referenced by several versions. Only
    // an object that *no* live version references may be deleted.

    /// Object ids that no live version references any more, whose dead versions
    /// are all older than `older_than`.
    ///
    /// The caller deletes each one from the object store and then calls
    /// [`MetadataRepository::forget_object`]. Deleting from the store first
    /// means a crash in between leaks a metadata row, not an unreachable
    /// object that nothing will ever collect.
    async fn collectable_objects(
        &self,
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Uuid>, RepositoryError>;

    /// Drop every chunk row pointing at an object that has been deleted from
    /// the store.
    async fn forget_object(&self, object_id: Uuid) -> Result<u64, RepositoryError>;

    /// Delete dead versions that have no chunks left, and returns how many.
    async fn purge_empty_dead_versions(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<u64, RepositoryError>;

    /// Hard-delete soft-deleted nodes that have no versions left.
    async fn purge_deleted_nodes(&self, older_than: DateTime<Utc>) -> Result<u64, RepositoryError>;
}
