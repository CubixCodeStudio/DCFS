//! In-memory object store for testing.

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use discordfs_core::ObjectId;
use parking_lot::RwLock;
use std::collections::HashMap;

use crate::{ObjectLocator, ObjectStore, ObjectStoreError, StoredObject};

/// In-memory object store for deterministic testing.
pub struct MemoryObjectStore {
    objects: RwLock<HashMap<ObjectId, (Bytes, chrono::DateTime<Utc>)>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self {
            objects: RwLock::new(HashMap::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.objects.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.read().is_empty()
    }
}

impl Default for MemoryObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ObjectStore for MemoryObjectStore {
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError> {
        let mut objects = self.objects.write();
        if objects.contains_key(&id) {
            return Err(ObjectStoreError::AlreadyExists(id));
        }
        let created_at = Utc::now();
        let size = data.len() as u64;
        objects.insert(id, (data, created_at));
        Ok(StoredObject {
            id,
            size,
            created_at,
        })
    }

    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError> {
        let objects = self.objects.read();
        objects
            .get(&locator.id)
            .map(|(data, _)| data.clone())
            .ok_or(ObjectStoreError::NotFound(locator.id))
    }

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

    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError> {
        let mut objects = self.objects.write();
        objects
            .remove(&locator.id)
            .ok_or(ObjectStoreError::NotFound(locator.id))?;
        Ok(())
    }

    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError> {
        let objects = self.objects.read();
        let (data, created_at) = objects
            .get(&locator.id)
            .ok_or(ObjectStoreError::NotFound(locator.id))?;
        Ok(StoredObject {
            id: locator.id,
            size: data.len() as u64,
            created_at: *created_at,
        })
    }
}
