//! Application state shared across handlers.

use discordfs_crypto::{EncryptionKey, KeyId};
use discordfs_db::MetadataRepository;
use discordfs_objectstore::ObjectStore;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Shared application state.
///
/// Both backends are trait objects so the same routes run against the in-memory
/// pair in tests and against PostgreSQL/Discord in production.
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn MetadataRepository>,
    pub store: Arc<dyn ObjectStore>,
    /// Key that every chunk is sealed with. The FUSE client never sees it.
    pub key: Arc<EncryptionKey>,
    /// Which key sealed a chunk. Bound as associated data, so it cannot be
    /// swapped after the fact; v0.1 has exactly one key and no rotation.
    pub key_id: KeyId,
    /// Chunk payload size for new versions.
    pub chunk_size: u64,
    /// Shared bearer token. `None` disables the check and is only reachable
    /// from tests; the binary refuses to start without one.
    pub api_token: Option<Arc<String>>,
    /// One lock per file being written.
    ///
    /// A write is read-modify-write across several requests to the metadata
    /// store, and the kernel issues writes to one file in parallel, so without
    /// this every concurrent write but one loses the optimistic-concurrency
    /// check and fails.
    ///
    /// ponytail: process-local, so it only serializes writers inside one
    /// server. Two servers sharing a database still need a PostgreSQL advisory
    /// lock on the node id; the commit guard keeps them correct meanwhile, just
    /// noisier.
    write_locks: Arc<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
}

impl AppState {
    pub fn new(
        repo: Arc<dyn MetadataRepository>,
        store: Arc<dyn ObjectStore>,
        master_key: [u8; 32],
        chunk_size: u64,
    ) -> Self {
        Self {
            repo,
            store,
            key: Arc::new(EncryptionKey::from_bytes(master_key)),
            key_id: KeyId::new("master"),
            chunk_size,
            api_token: None,
            write_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The write lock for one node, created on demand.
    pub fn write_lock(&self, node_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.write_locks.lock();
        // Nobody is waiting on the locks that only the map still holds, so this
        // is the cheap moment to drop them and keep the map bounded.
        if locks.len() > 1024 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        locks.entry(node_id).or_default().clone()
    }

    /// Require this bearer token on every non-health request.
    pub fn with_api_token(mut self, token: impl Into<String>) -> Self {
        self.api_token = Some(Arc::new(token.into()));
        self
    }
}
