//! Object store backed by a local directory.
//!
//! Durable without a Discord bot, which is what makes a local deployment
//! usable: the in-memory store loses every chunk on restart while the metadata
//! store keeps pointing at them.
//!
//! Objects hold ciphertext, so the directory does not contain readable file
//! contents — but it does hold everything needed to decrypt them given the
//! master key, so protect it like any other data directory.

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use discordfs_core::ObjectId;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{ObjectLocator, ObjectStore, ObjectStoreError, StoredObject};

pub struct FsObjectStore {
    root: PathBuf,
}

impl FsObjectStore {
    /// Open (and create) the object directory.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, ObjectStoreError> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await.map_err(|e| {
            ObjectStoreError::Backend(format!("cannot create {}: {e}", root.display()))
        })?;
        Ok(Self { root })
    }

    /// Objects are fanned out by the first two hex characters of their id so a
    /// single directory never holds millions of entries.
    fn path_of(&self, id: &ObjectId) -> PathBuf {
        let name = id.as_uuid().simple().to_string();
        self.root.join(&name[..2]).join(name)
    }
}

fn io_err(e: std::io::Error, id: ObjectId) -> ObjectStoreError {
    match e.kind() {
        ErrorKind::NotFound => ObjectStoreError::NotFound(id),
        _ => ObjectStoreError::Backend(e.to_string()),
    }
}

async fn created_at(path: &Path) -> DateTime<Utc> {
    match tokio::fs::metadata(path).await.and_then(|m| m.modified()) {
        Ok(time) => DateTime::from(time),
        // The timestamp is informational; never fail a read over it.
        Err(_) => Utc::now(),
    }
}

#[async_trait]
impl ObjectStore for FsObjectStore {
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError> {
        let path = self.path_of(&id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_err(e, id))?;
        }
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            // Objects are immutable: a repeated id is a bug, not an update.
            return Err(ObjectStoreError::AlreadyExists(id));
        }

        // Write to a temporary name and rename into place, so a crash mid-write
        // never leaves a short object that reads as valid.
        let temp = path.with_extension("partial");
        tokio::fs::write(&temp, &data)
            .await
            .map_err(|e| io_err(e, id))?;
        tokio::fs::rename(&temp, &path)
            .await
            .map_err(|e| io_err(e, id))?;

        Ok(StoredObject {
            id,
            size: data.len() as u64,
            created_at: Utc::now(),
        })
    }

    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError> {
        let path = self.path_of(&locator.id);
        let data = tokio::fs::read(&path)
            .await
            .map_err(|e| io_err(e, locator.id))?;
        Ok(Bytes::from(data))
    }

    async fn get_range(
        &self,
        locator: &ObjectLocator,
        offset: u64,
        len: u64,
    ) -> Result<Bytes, ObjectStoreError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let path = self.path_of(&locator.id);
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_err(e, locator.id))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| io_err(e, locator.id))?;

        // Short reads at the end of the object are the caller's to interpret,
        // so read what is there rather than insisting on the full length.
        let mut buf = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(io_err(e, locator.id)),
            }
        }
        buf.truncate(filled);
        Ok(Bytes::from(buf))
    }

    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError> {
        match tokio::fs::remove_file(self.path_of(&locator.id)).await {
            Ok(()) => Ok(()),
            // Deleting what is already gone is the desired end state.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e, locator.id)),
        }
    }

    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError> {
        let path = self.path_of(&locator.id);
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|e| io_err(e, locator.id))?;
        Ok(StoredObject {
            id: locator.id,
            size: metadata.len(),
            created_at: created_at(&path).await,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn objects_are_immutable_and_survive_reopening() {
        let dir = std::env::temp_dir().join(format!("dfs-store-{}", uuid_like()));
        let store = FsObjectStore::open(&dir).await.unwrap();

        let id = ObjectId::new();
        store
            .put(id, Bytes::from_static(b"ciphertext"))
            .await
            .unwrap();

        let locator = ObjectLocator::new(id);
        assert_eq!(store.get(&locator).await.unwrap(), "ciphertext");
        assert_eq!(store.stat(&locator).await.unwrap().size, 10);

        // Same id again must be refused rather than overwriting.
        assert!(matches!(
            store.put(id, Bytes::from_static(b"other")).await,
            Err(ObjectStoreError::AlreadyExists(_))
        ));

        // A fresh handle on the same directory still finds it: this is the
        // property the in-memory store does not have.
        let reopened = FsObjectStore::open(&dir).await.unwrap();
        assert_eq!(reopened.get(&locator).await.unwrap(), "ciphertext");

        reopened.delete(&locator).await.unwrap();
        assert!(matches!(
            reopened.get(&locator).await,
            Err(ObjectStoreError::NotFound(_))
        ));
        // Deleting twice is not an error.
        reopened.delete(&locator).await.unwrap();

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// A unique-enough suffix without pulling `uuid` into this crate's deps.
    fn uuid_like() -> u128 {
        ObjectId::new().as_uuid().as_u128()
    }
}
