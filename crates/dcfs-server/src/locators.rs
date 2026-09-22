//! Object locators, kept in the metadata store.
//!
//! This is what makes a remote backend survive a restart: Discord hands back a
//! message and attachment id when something is uploaded, and without that pair
//! recorded somewhere durable the upload can never be found again.

use async_trait::async_trait;
use dcfs_core::ObjectId;
use dcfs_db::{MetadataRepository, ObjectLocatorRecord, RepositoryError};
use dcfs_discord::{DiscordLocator, LocatorStore};
use std::sync::Arc;

pub struct RepositoryLocatorStore {
    repo: Arc<dyn MetadataRepository>,
}

/// Names the backend, and which webhook of it, in the one column the schema
/// has for the question: `discord` on its own, or `discord:<webhook id>`.
///
/// A webhook can only fetch and delete its own messages, so once there is more
/// than one an object that does not say which wrote it cannot be reached. The
/// bare form is what single-webhook deployments already wrote, and reads back
/// as "whichever webhook this is", which is the truth for them.
const BACKEND: &str = "discord";

impl RepositoryLocatorStore {
    pub fn new(repo: Arc<dyn MetadataRepository>) -> Self {
        Self { repo }
    }
}

#[async_trait]
impl LocatorStore for RepositoryLocatorStore {
    async fn put(&self, locator: &DiscordLocator) -> Result<(), String> {
        self.repo
            .put_object_locator(&ObjectLocatorRecord {
                object_id: *locator.object_id.as_uuid(),
                backend: if locator.webhook_id.is_empty() {
                    BACKEND.to_string()
                } else {
                    format!("{BACKEND}:{}", locator.webhook_id)
                },
                message_id: locator.message_id.clone(),
                attachment_id: locator.attachment_id.clone(),
                url: locator.url.clone(),
                size: locator.size as i64,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn get(&self, id: ObjectId) -> Result<Option<DiscordLocator>, String> {
        match self.repo.get_object_locator(*id.as_uuid()).await {
            Ok(record) => {
                let webhook = record
                    .backend
                    .split_once(':')
                    .map(|(_, webhook)| webhook)
                    .unwrap_or_default();
                Ok(Some(
                    DiscordLocator::new(
                        id,
                        record.message_id,
                        record.attachment_id,
                        record.url,
                        record.size.max(0) as u64,
                    )
                    .from_webhook(webhook),
                ))
            }
            // An object nobody recorded is simply not there.
            Err(RepositoryError::NotFound) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn set_url(&self, id: ObjectId, url: &str) -> Result<(), String> {
        self.repo
            .touch_object_url(*id.as_uuid(), url)
            .await
            .map_err(|e| e.to_string())
    }

    async fn remove(&self, id: ObjectId) -> Result<(), String> {
        self.repo
            .delete_object_locator(*id.as_uuid())
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcfs_db::MemoryMetadataRepository;

    #[tokio::test]
    async fn a_locator_survives_being_written_and_read_back() {
        let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
        let store = RepositoryLocatorStore::new(repo.clone());
        let id = ObjectId::new();

        assert!(store.get(id).await.unwrap().is_none());

        let locator = DiscordLocator::new(
            id,
            "msg_1".into(),
            "att_1".into(),
            "https://cdn.example/expiring".into(),
            4096,
        );
        store.put(&locator).await.unwrap();

        // A second handle on the same repository finds it: this is the property
        // an in-memory map does not have.
        let reopened = RepositoryLocatorStore::new(repo);
        let found = reopened.get(id).await.unwrap().expect("recorded");
        assert_eq!(found.message_id, "msg_1");
        assert_eq!(found.attachment_id, "att_1");
        assert_eq!(found.size, 4096);

        // A refreshed URL replaces the cached one; the ids never change.
        reopened
            .set_url(id, "https://cdn.example/fresh")
            .await
            .unwrap();
        let found = reopened.get(id).await.unwrap().unwrap();
        assert_eq!(found.url, "https://cdn.example/fresh");
        assert_eq!(found.message_id, "msg_1");

        reopened.remove(id).await.unwrap();
        assert!(reopened.get(id).await.unwrap().is_none());
    }
}

#[cfg(test)]
mod webhook_tests {
    use super::*;
    use dcfs_db::MemoryMetadataRepository;

    /// Which webhook holds an object has to survive a restart, or with more
    /// than one configured the object cannot be fetched or deleted again.
    #[tokio::test]
    async fn the_webhook_that_wrote_an_object_is_remembered() {
        let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
        let store = RepositoryLocatorStore::new(repo);

        let id = ObjectId::new();
        store
            .put(
                &DiscordLocator::new(id, "m1".into(), "a1".into(), "https://cdn/x".into(), 9)
                    .from_webhook("hook_7"),
            )
            .await
            .unwrap();

        let back = store.get(id).await.unwrap().expect("recorded");
        assert_eq!(back.webhook_id, "hook_7");
        assert_eq!(back.message_id, "m1");
    }

    /// Objects written when there was only one webhook say nothing about
    /// which, and must read back as "whichever this is" rather than failing.
    #[tokio::test]
    async fn an_object_from_before_webhooks_were_named_still_reads() {
        let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
        let id = ObjectId::new();
        repo.put_object_locator(&ObjectLocatorRecord {
            object_id: *id.as_uuid(),
            backend: "discord".to_string(),
            message_id: "m0".to_string(),
            attachment_id: "a0".to_string(),
            url: "https://cdn/old".to_string(),
            size: 4,
        })
        .await
        .unwrap();

        let store = RepositoryLocatorStore::new(repo);
        let back = store.get(id).await.unwrap().expect("recorded");
        assert_eq!(back.webhook_id, "", "nothing recorded, and that is fine");
        assert_eq!(back.message_id, "m0");
    }
}
