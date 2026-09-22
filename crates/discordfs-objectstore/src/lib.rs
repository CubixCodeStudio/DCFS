//! Object Store abstraction for DiscordFS.
//!
//! Objects are immutable encrypted chunks stored remotely.

pub mod fs;
pub mod memory;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use discordfs_core::ObjectId;
use thiserror::Error;

/// Error from object store operations.
#[derive(Debug, Error)]
pub enum ObjectStoreError {
    #[error("object not found: {0}")]
    NotFound(ObjectId),
    #[error("object already exists: {0}")]
    AlreadyExists(ObjectId),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A stored object's metadata.
#[derive(Debug, Clone)]
pub struct StoredObject {
    pub id: ObjectId,
    pub size: u64,
    pub created_at: DateTime<Utc>,
}

/// Locator for retrieving an object.
#[derive(Debug, Clone)]
pub struct ObjectLocator {
    pub id: ObjectId,
}

impl ObjectLocator {
    pub fn new(id: ObjectId) -> Self {
        Self { id }
    }
}

pub use fs::FsObjectStore;
pub use memory::MemoryObjectStore;

/// Trait for immutable object storage backends.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// Store a new immutable object.
    ///
    /// Returns an error if an object with the same ID already exists.
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError>;

    /// Retrieve an object's data.
    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError>;

    /// Store a new object, naming what it belongs with.
    ///
    /// A backend spread over several endpoints uses this to keep everything in
    /// a group together — the server groups by file. Backends where placement
    /// means nothing ignore it.
    async fn put_for(
        &self,
        id: ObjectId,
        data: Bytes,
        _group: uuid::Uuid,
    ) -> Result<StoredObject, ObjectStoreError> {
        self.put(id, data).await
    }

    /// Retrieve `len` bytes of an object starting at `offset`.
    ///
    /// The default pulls the whole object and slices it, which is correct but
    /// costs the same as [`ObjectStore::get`]. Backends that can serve part of
    /// an object should override it: reading a few bytes out of the middle of
    /// a large file is what this exists for.
    async fn get_range(
        &self,
        locator: &ObjectLocator,
        offset: u64,
        len: u64,
    ) -> Result<Bytes, ObjectStoreError> {
        let all = self.get(locator).await?;
        let start = (offset as usize).min(all.len());
        let end = (start + len as usize).min(all.len());
        Ok(all.slice(start..end))
    }

    /// Delete an object.
    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError>;

    /// Get object metadata without downloading.
    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError>;
}
