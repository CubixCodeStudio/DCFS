//! Discord-backed ObjectStore implementation.

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use discordfs_core::ObjectId;
use discordfs_objectstore::{ObjectLocator, ObjectStore, ObjectStoreError, StoredObject};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::client::{DiscordClient, DiscordError};
use crate::locator::DiscordLocator;

/// Where the mapping from object id to Discord message lives.
///
/// Discord gives back a message and attachment id when something is uploaded,
/// and nothing else can find that upload again. Keeping the mapping in memory
/// means every object becomes unreachable when the process restarts, so this is
/// a trait: production persists it, tests do not.
#[async_trait]
pub trait LocatorStore: Send + Sync + 'static {
    async fn put(&self, locator: &DiscordLocator) -> Result<(), String>;
    async fn get(&self, id: ObjectId) -> Result<Option<DiscordLocator>, String>;
    /// Record a freshly issued CDN URL for an object.
    async fn set_url(&self, id: ObjectId, url: &str) -> Result<(), String>;
    /// Forget an object after its message has been deleted.
    async fn remove(&self, id: ObjectId) -> Result<(), String>;
}

/// Keeps locators for the life of the process. Tests only: an object stored
/// through this cannot be found again after a restart.
#[derive(Default)]
pub struct MemoryLocatorStore {
    locators: RwLock<HashMap<ObjectId, DiscordLocator>>,
}

#[async_trait]
impl LocatorStore for MemoryLocatorStore {
    async fn put(&self, locator: &DiscordLocator) -> Result<(), String> {
        self.locators
            .write()
            .insert(locator.object_id, locator.clone());
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<Option<DiscordLocator>, String> {
        Ok(self.locators.read().get(&id).cloned())
    }

    async fn set_url(&self, id: ObjectId, url: &str) -> Result<(), String> {
        if let Some(locator) = self.locators.write().get_mut(&id) {
            locator.url = url.to_string();
        }
        Ok(())
    }

    async fn remove(&self, id: ObjectId) -> Result<(), String> {
        self.locators.write().remove(&id);
        Ok(())
    }
}

/// ObjectStore backed by Discord webhook attachments.
///
/// Objects are uploaded as attachments. The durable part of a locator is the
/// message and attachment id; the CDN URL is a cache, because Discord's links
/// expire, and a download that fails is retried once against a freshly fetched
/// URL before it is reported as an error.
pub struct DiscordObjectStore {
    /// One per configured webhook. Never empty.
    ///
    /// More of them buys upload capacity: each is rate limited on its own, and
    /// a large upload is bounded by one webhook's share rather than the
    /// account's. Which one holds an object is recorded, because a webhook can
    /// only fetch and delete what it posted itself.
    clients: Vec<DiscordClient>,
    locators: Arc<dyn LocatorStore>,
}

impl DiscordObjectStore {
    /// A store whose locators live only in memory. Tests and experiments.
    pub fn new(client: DiscordClient) -> Self {
        Self::with_locator_store(client, Arc::new(MemoryLocatorStore::default()))
    }

    pub fn with_locator_store(client: DiscordClient, locators: Arc<dyn LocatorStore>) -> Self {
        Self::with_clients(vec![client], locators)
    }

    /// Spread objects over several webhooks.
    pub fn with_clients(clients: Vec<DiscordClient>, locators: Arc<dyn LocatorStore>) -> Self {
        assert!(!clients.is_empty(), "a Discord store needs a webhook");
        Self { clients, locators }
    }

    /// The first configured client. With one webhook that is the whole store.
    pub fn client(&self) -> &DiscordClient {
        &self.clients[0]
    }

    /// Which webhook a new object goes to.
    ///
    /// Everything sharing a group lands on the same one, and the caller groups
    /// by file: spreading one file's parts over several webhooks would buy
    /// nothing — the upload is bounded by the link, not the webhook — and
    /// would scatter a single file across several channels to no purpose.
    fn client_for_group(&self, group: Uuid) -> &DiscordClient {
        // The low bytes of a v4 uuid are random, which is all the spread this
        // needs; the mapping only has to be stable within one call.
        let pick = u64::from_le_bytes(group.as_bytes()[8..16].try_into().unwrap());
        &self.clients[(pick % self.clients.len() as u64) as usize]
    }

    /// The webhook that holds an object, by what was recorded when it was
    /// written.
    ///
    /// Placement is a hash, but retrieval must never be: webhooks can be added
    /// or removed, and the hash would then point somewhere the object is not.
    /// An object written when there was one webhook has nothing recorded, and
    /// the first client is the one that wrote it.
    fn client_for(&self, locator: &DiscordLocator) -> &DiscordClient {
        self.clients
            .iter()
            .find(|c| c.webhook_id() == locator.webhook_id)
            .unwrap_or(&self.clients[0])
    }

    async fn locator_of(&self, id: ObjectId) -> Result<DiscordLocator, ObjectStoreError> {
        self.locators
            .get(id)
            .await
            .map_err(ObjectStoreError::Backend)?
            .ok_or(ObjectStoreError::NotFound(id))
    }

    /// Ask Discord for the message again and take the current URL of the
    /// attachment the locator names.
    /// The URL to download from, refreshed first if it has already expired.
    ///
    /// A signed CDN link carries its own expiry, so a link known to be dead
    /// can be replaced without spending a download to discover it. Reading a
    /// file stored a day ago used to fail once per part before refreshing;
    /// this turns that into one refresh per part and no wasted fetch.
    ///
    /// Only an expiry that is certainly past is acted on. A link with no
    /// readable expiry is used as before and refreshed if it fails, so this
    /// is an optimisation and never the thing correctness rests on.
    async fn fresh_url(&self, locator: &DiscordLocator) -> Result<String, ObjectStoreError> {
        match url_expiry(&locator.url) {
            Some(expiry) if expiry <= Utc::now() + chrono::Duration::seconds(60) => {
                tracing::debug!(object_id = %locator.object_id, %expiry, "link expired; refreshing");
                self.refresh_url(locator).await
            }
            _ => Ok(locator.url.clone()),
        }
    }

    async fn refresh_url(&self, locator: &DiscordLocator) -> Result<String, ObjectStoreError> {
        let message = self
            .client_for(locator)
            .fetch_message(&locator.message_id)
            .await
            .map_err(|e| map_err(e, locator.object_id))?;
        let attachment = message
            .attachments
            .into_iter()
            .find(|a| a.id == locator.attachment_id)
            .ok_or(ObjectStoreError::NotFound(locator.object_id))?;
        self.locators
            .set_url(locator.object_id, &attachment.url)
            .await
            .map_err(ObjectStoreError::Backend)?;
        Ok(attachment.url)
    }
}

/// When a signed CDN link stops working, read out of the link itself.
///
/// Discord signs an attachment URL with `ex`, the expiry as hex seconds since
/// the epoch. It is not part of the documented API, so a link that does not
/// carry one, or carries something unreadable, is treated as "no idea" and
/// used as it is.
fn url_expiry(url: &str) -> Option<DateTime<Utc>> {
    let query = url.split_once('?')?.1;
    let raw = query.split('&').find_map(|pair| pair.strip_prefix("ex="))?;
    let seconds = i64::from_str_radix(raw, 16).ok()?;
    DateTime::from_timestamp(seconds, 0)
}

fn map_err(e: DiscordError, id: ObjectId) -> ObjectStoreError {
    match e {
        DiscordError::Api { status: 404, .. } => ObjectStoreError::NotFound(id),
        _ => ObjectStoreError::Backend(e.to_string()),
    }
}

#[async_trait]
impl ObjectStore for DiscordObjectStore {
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError> {
        // No grouping asked for: the object is its own group.
        self.put_for(id, data, *id.as_uuid()).await
    }

    async fn put_for(
        &self,
        id: ObjectId,
        data: Bytes,
        group: Uuid,
    ) -> Result<StoredObject, ObjectStoreError> {
        // Objects are immutable: a repeated id is a bug, not an update.
        if self
            .locators
            .get(id)
            .await
            .map_err(ObjectStoreError::Backend)?
            .is_some()
        {
            return Err(ObjectStoreError::AlreadyExists(id));
        }

        let filename = format!("{id}.bin");
        let client = self.client_for_group(group);
        let msg = client
            .upload_attachment(&filename, data.clone())
            .await
            .map_err(|e| map_err(e, id))?;

        let attachment = msg.attachments.into_iter().next().ok_or_else(|| {
            ObjectStoreError::Backend("no attachment in the response".to_string())
        })?;

        let locator =
            DiscordLocator::new(id, msg.id, attachment.id, attachment.url, attachment.size)
                .from_webhook(client.webhook_id());
        // Record where it went before reporting success: an upload nobody can
        // find again is worse than an upload that failed.
        self.locators
            .put(&locator)
            .await
            .map_err(ObjectStoreError::Backend)?;

        Ok(StoredObject {
            id,
            size: attachment.size,
            created_at: Utc::now(),
        })
    }

    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError> {
        let discord_loc = self.locator_of(locator.id).await?;
        let url = self.fresh_url(&discord_loc).await?;

        match self
            .client_for(&discord_loc)
            .download_attachment(&url)
            .await
        {
            Ok(bytes) => Ok(bytes),
            Err(first) => {
                // Discord's CDN links expire, so a failure here is usually a
                // stale URL rather than a missing object. Ask the message for
                // the current one and try once more before giving up.
                tracing::debug!(
                    object_id = %locator.id,
                    "cached attachment URL failed ({first}); refreshing"
                );
                let fresh = self.refresh_url(&discord_loc).await?;
                self.client_for(&discord_loc)
                    .download_attachment(&fresh)
                    .await
                    .map_err(|e| map_err(e, locator.id))
            }
        }
    }

    async fn get_range(
        &self,
        locator: &ObjectLocator,
        offset: u64,
        len: u64,
    ) -> Result<Bytes, ObjectStoreError> {
        let discord_loc = self.locator_of(locator.id).await?;
        let url = self.fresh_url(&discord_loc).await?;
        let range = Some((offset, len));

        match self
            .client_for(&discord_loc)
            .download_attachment_range(&url, range)
            .await
        {
            Ok(bytes) => Ok(bytes),
            Err(first) => {
                tracing::debug!(
                    object_id = %locator.id,
                    "cached attachment URL failed ({first}); refreshing"
                );
                let fresh = self.refresh_url(&discord_loc).await?;
                self.client_for(&discord_loc)
                    .download_attachment_range(&fresh, range)
                    .await
                    .map_err(|e| map_err(e, locator.id))
            }
        }
    }

    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError> {
        let discord_loc = self.locator_of(locator.id).await?;
        match self
            .client_for(&discord_loc)
            .delete_message(&discord_loc.message_id)
            .await
        {
            // Already gone is the desired end state.
            Ok(()) | Err(DiscordError::Api { status: 404, .. }) => {}
            Err(e) => return Err(map_err(e, locator.id)),
        }
        // Drop the locator last: while it exists the object is still findable,
        // so a failure above leaves something to retry rather than an orphan.
        self.locators
            .remove(locator.id)
            .await
            .map_err(ObjectStoreError::Backend)
    }

    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError> {
        let discord_loc = self.locator_of(locator.id).await?;
        Ok(StoredObject {
            id: locator.id,
            size: discord_loc.size,
            created_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DiscordClientConfig;
    use mockito::Server;

    fn test_client(server_url: &str) -> DiscordClient {
        let config = DiscordClientConfig::new("test_id", "test_token").with_base_url(server_url);
        DiscordClient::new(config)
    }

    #[tokio::test]
    async fn put_and_get_roundtrip() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "id": "msg_001",
                "attachments": [{
                    "id": "att_001",
                    "filename": "test.bin",
                    "size": 13,
                    "url": "DOWNLOAD_URL",
                    "proxy_url": "https://proxy/att_001"
                }]
            }"#
                .replace("DOWNLOAD_URL", &format!("{}/download/test", server.url())),
            )
            .create_async()
            .await;

        let download_mock = server
            .mock("GET", "/download/test")
            .with_status(200)
            .with_body("hello, world!")
            .create_async()
            .await;

        let client = test_client(&server.url());
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        let data = Bytes::from("hello, world!");
        let stored = store.put(id, data.clone()).await.unwrap();
        assert_eq!(stored.id, id);
        assert_eq!(stored.size, 13);

        let locator = ObjectLocator::new(id);
        let fetched = store.get(&locator).await.unwrap();
        assert_eq!(fetched, data);

        mock.assert_async().await;
        download_mock.assert_async().await;
    }

    #[tokio::test]
    async fn put_already_exists() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "id": "msg_002",
                "attachments": [{
                    "id": "att_002",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://x",
                    "proxy_url": "http://y"
                }]
            }"#,
            )
            .create_async()
            .await;

        let client = test_client(&server.url());
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        store.put(id, Bytes::from("test")).await.unwrap();
        let result = store.put(id, Bytes::from("test2")).await;
        assert!(matches!(result, Err(ObjectStoreError::AlreadyExists(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn get_not_found() {
        let client = test_client("http://unused");
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        let locator = ObjectLocator::new(id);
        let result = store.get(&locator).await;
        assert!(matches!(result, Err(ObjectStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_removes_locator() {
        let mut server = Server::new_async().await;
        let upload_mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "id": "msg_003",
                "attachments": [{
                    "id": "att_003",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://x",
                    "proxy_url": "http://y"
                }]
            }"#,
            )
            .create_async()
            .await;

        let delete_mock = server
            .mock("DELETE", "/webhooks/test_id/test_token/messages/msg_003")
            .with_status(204)
            .create_async()
            .await;

        let client = test_client(&server.url());
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        store.put(id, Bytes::from("test")).await.unwrap();

        let locator = ObjectLocator::new(id);
        assert!(store.stat(&locator).await.is_ok());
        store.delete(&locator).await.unwrap();
        // The locator goes with the message: nothing points at it any more.
        assert!(matches!(
            store.stat(&locator).await,
            Err(ObjectStoreError::NotFound(_))
        ));

        upload_mock.assert_async().await;
        delete_mock.assert_async().await;
    }

    /// Discord's attachment URLs expire. The locator keeps the message and
    /// attachment ids, which do not, and a download that fails is retried once
    /// against a URL fetched fresh from the message.
    #[tokio::test]
    async fn an_expired_url_is_refreshed_from_the_message() {
        let mut server = Server::new_async().await;
        let stale = format!("{}/cdn/stale", server.url());
        let fresh = format!("{}/cdn/fresh", server.url());

        let upload = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_exp","attachments":[{{"id":"att_exp","filename":"o.bin","size":5,"url":"{stale}","proxy_url":"p"}}]}}"#
            ))
            .create_async()
            .await;

        // The cached link is dead, as an expired Discord link is.
        let dead = server
            .mock("GET", "/cdn/stale")
            .with_status(403)
            .expect(1)
            .create_async()
            .await;
        // Re-reading the message yields a working one.
        let refetch = server
            .mock("GET", "/webhooks/test_id/test_token/messages/msg_exp")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_exp","attachments":[{{"id":"att_exp","filename":"o.bin","size":5,"url":"{fresh}","proxy_url":"p"}}]}}"#
            ))
            .expect(1)
            .create_async()
            .await;
        let alive = server
            .mock("GET", "/cdn/fresh")
            .with_status(200)
            .with_body("bytes")
            .expect(2)
            .create_async()
            .await;

        let store = DiscordObjectStore::new(test_client(&server.url()));
        let id = ObjectId::new();
        store.put(id, Bytes::from("bytes")).await.unwrap();

        let locator = ObjectLocator::new(id);
        assert_eq!(store.get(&locator).await.unwrap(), Bytes::from("bytes"));

        // The refreshed URL is remembered, so the next read does not refetch.
        assert_eq!(store.get(&locator).await.unwrap(), Bytes::from("bytes"));

        upload.assert_async().await;
        dead.assert_async().await;
        refetch.assert_async().await;
        alive.assert_async().await;
    }

    /// A locator store that forgets nothing across handles stands in for the
    /// metadata store the server uses.
    #[tokio::test]
    async fn a_shared_locator_store_outlives_one_store_handle() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_shared","attachments":[{{"id":"att_shared","filename":"o.bin","size":4,"url":"{}/dl","proxy_url":"p"}}]}}"#,
                server.url()
            ))
            .create_async()
            .await;
        let download = server
            .mock("GET", "/dl")
            .with_status(200)
            .with_body("keep")
            .create_async()
            .await;

        let locators = Arc::new(MemoryLocatorStore::default());
        let id = ObjectId::new();
        {
            let store = DiscordObjectStore::with_locator_store(
                test_client(&server.url()),
                locators.clone(),
            );
            store.put(id, Bytes::from("keep")).await.unwrap();
        }

        // A new store over the same locators still finds the object: this is
        // what the metadata-backed store buys in production.
        let reopened = DiscordObjectStore::with_locator_store(test_client(&server.url()), locators);
        let fetched = reopened.get(&ObjectLocator::new(id)).await.unwrap();
        assert_eq!(fetched, Bytes::from("keep"));

        mock.assert_async().await;
        download.assert_async().await;
    }

    #[tokio::test]
    async fn stat_returns_metadata() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "id": "msg_004",
                "attachments": [{
                    "id": "att_004",
                    "filename": "test.bin",
                    "size": 42,
                    "url": "http://x",
                    "proxy_url": "http://y"
                }]
            }"#,
            )
            .create_async()
            .await;

        let client = test_client(&server.url());
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        store.put(id, Bytes::from(vec![0u8; 42])).await.unwrap();

        let locator = ObjectLocator::new(id);
        let meta = store.stat(&locator).await.unwrap();
        assert_eq!(meta.id, id);
        assert_eq!(meta.size, 42);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn retry_on_500() {
        let mut server = Server::new_async().await;
        let fail_mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(500)
            .with_body("internal error")
            .expect(3)
            .create_async()
            .await;

        let config = DiscordClientConfig::new("test_id", "test_token")
            .with_base_url(server.url())
            .with_retry(crate::retry::RetryConfig {
                max_attempts: 2,
                initial_backoff: std::time::Duration::from_millis(10),
                max_backoff: std::time::Duration::from_millis(50),
                multiplier: 2.0,
            });
        let client = DiscordClient::new(config);
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        let result = store.put(id, Bytes::from("test")).await;
        assert!(result.is_err());

        fail_mock.assert_async().await;
    }

    #[tokio::test]
    async fn retry_on_429_then_success() {
        let mut server = Server::new_async().await;

        let rate_limit_mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(429)
            .with_header("retry-after", "0.01")
            .with_body(r#"{"message": "rate limited", "code": 429}"#)
            .expect(1)
            .create_async()
            .await;

        let success_mock = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "id": "msg_ok",
                "attachments": [{
                    "id": "att_ok",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://x",
                    "proxy_url": "http://y"
                }]
            }"#,
            )
            .expect(1)
            .create_async()
            .await;

        let config = DiscordClientConfig::new("test_id", "test_token")
            .with_base_url(server.url())
            .with_retry(crate::retry::RetryConfig {
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(10),
                max_backoff: std::time::Duration::from_millis(50),
                multiplier: 2.0,
            });
        let client = DiscordClient::new(config);
        let store = DiscordObjectStore::new(client);

        let id = ObjectId::new();
        let result = store.put(id, Bytes::from("test")).await;
        assert!(result.is_ok());

        rate_limit_mock.assert_async().await;
        success_mock.assert_async().await;
    }

    /// Reading a file stored a day ago should not spend a failed download per
    /// part to discover what the link already says.
    #[tokio::test]
    async fn a_link_that_has_expired_is_refreshed_without_being_tried() {
        let mut server = Server::new_async().await;
        let dead = format!(
            "{}/dead?ex={:x}&is=a&hm=b",
            server.url(),
            (Utc::now() - chrono::Duration::hours(1)).timestamp()
        );
        let alive = format!(
            "{}/alive?ex={:x}&is=a&hm=b",
            server.url(),
            (Utc::now() + chrono::Duration::hours(24)).timestamp()
        );

        let upload = server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_1","attachments":[{{"id":"att_1","filename":"o.bin","size":4,"url":"{dead}","proxy_url":"p"}}]}}"#
            ))
            .create_async()
            .await;
        // The message now reports a live link.
        let refresh = server
            .mock("GET", "/webhooks/test_id/test_token/messages/msg_1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_1","attachments":[{{"id":"att_1","filename":"o.bin","size":4,"url":"{alive}","proxy_url":"p"}}]}}"#
            ))
            .expect(1)
            .create_async()
            .await;
        let dead_fetch = server
            .mock("GET", "/dead")
            .match_query(mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let live_fetch = server
            .mock("GET", "/alive")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body("keep")
            .expect(1)
            .create_async()
            .await;

        let store = DiscordObjectStore::new(test_client(&server.url()));
        let id = ObjectId::new();
        store.put(id, Bytes::from("keep")).await.unwrap();

        assert_eq!(
            store.get(&ObjectLocator::new(id)).await.unwrap(),
            Bytes::from("keep")
        );

        upload.assert_async().await;
        refresh.assert_async().await;
        dead_fetch.assert_async().await;
        live_fetch.assert_async().await;
    }

    /// A link with life left in it is used as it is: no refresh, one fetch.
    #[tokio::test]
    async fn a_live_link_is_not_refreshed() {
        let mut server = Server::new_async().await;
        let alive = format!(
            "{}/alive?ex={:x}&is=a&hm=b",
            server.url(),
            (Utc::now() + chrono::Duration::hours(24)).timestamp()
        );

        server
            .mock("POST", "/webhooks/test_id/test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"id":"msg_2","attachments":[{{"id":"att_2","filename":"o.bin","size":4,"url":"{alive}","proxy_url":"p"}}]}}"#
            ))
            .create_async()
            .await;
        let refresh = server
            .mock("GET", "/webhooks/test_id/test_token/messages/msg_2")
            .expect(0)
            .create_async()
            .await;
        let live_fetch = server
            .mock("GET", "/alive")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body("keep")
            .expect(1)
            .create_async()
            .await;

        let store = DiscordObjectStore::new(test_client(&server.url()));
        let id = ObjectId::new();
        store.put(id, Bytes::from("keep")).await.unwrap();
        store.get(&ObjectLocator::new(id)).await.unwrap();

        refresh.assert_async().await;
        live_fetch.assert_async().await;
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;

    fn signed(ex: i64) -> String {
        format!("https://cdn.discordapp.com/attachments/1/2/p.bin?ex={ex:x}&is=abc&hm=def")
    }

    #[test]
    fn the_expiry_is_read_out_of_the_link() {
        let at = Utc::now() + chrono::Duration::hours(24);
        let parsed = url_expiry(&signed(at.timestamp())).expect("an ex parameter is readable");
        assert_eq!(parsed.timestamp(), at.timestamp());
    }

    /// A link Discord did not sign, or signed in a way this does not
    /// understand, must read as "no idea" rather than as expired — otherwise
    /// every read would refresh first.
    #[test]
    fn a_link_without_a_readable_expiry_is_left_alone() {
        for url in [
            "https://cdn.discordapp.com/attachments/1/2/p.bin",
            "https://cdn.discordapp.com/attachments/1/2/p.bin?is=abc",
            "https://cdn.discordapp.com/attachments/1/2/p.bin?ex=notahexnumber",
            "https://example.test/plain",
        ] {
            assert!(url_expiry(url).is_none(), "{url}");
        }
    }

    #[test]
    fn an_expired_link_is_recognised() {
        let past = Utc::now() - chrono::Duration::hours(1);
        assert!(url_expiry(&signed(past.timestamp())).unwrap() <= Utc::now());
    }
}

#[cfg(test)]
mod webhook_spread_tests {
    use super::*;
    use crate::client::DiscordClientConfig;
    use mockito::Server;

    /// Everything belonging to one file goes to one webhook, and objects of
    /// different files do not all pile onto the same one.
    #[test]
    fn a_file_goes_to_one_webhook_and_files_spread_over_them() {
        let clients: Vec<DiscordClient> = (0..4)
            .map(|i| {
                DiscordClient::new(DiscordClientConfig::new(
                    format!("hook{i}"),
                    format!("token{i}"),
                ))
            })
            .collect();
        let store =
            DiscordObjectStore::with_clients(clients, Arc::new(MemoryLocatorStore::default()));

        let file = Uuid::new_v4();
        let chosen = store.client_for_group(file).webhook_id().to_string();
        for _ in 0..20 {
            assert_eq!(
                store.client_for_group(file).webhook_id(),
                chosen,
                "one file never moves between webhooks"
            );
        }

        let used: std::collections::HashSet<String> = (0..200)
            .map(|_| {
                store
                    .client_for_group(Uuid::new_v4())
                    .webhook_id()
                    .to_string()
            })
            .collect();
        assert!(used.len() > 1, "files land on more than one webhook");
    }

    /// Placement is a hash, but finding an object again must not be: webhooks
    /// come and go, and the hash would then point where the object is not.
    #[test]
    fn an_object_is_read_from_the_webhook_that_wrote_it() {
        let clients: Vec<DiscordClient> = (0..3)
            .map(|i| {
                DiscordClient::new(DiscordClientConfig::new(
                    format!("hook{i}"),
                    format!("token{i}"),
                ))
            })
            .collect();
        let store =
            DiscordObjectStore::with_clients(clients, Arc::new(MemoryLocatorStore::default()));

        let locator = DiscordLocator::new(
            ObjectId::new(),
            "m".into(),
            "a".into(),
            "https://cdn/x".into(),
            1,
        )
        .from_webhook("hook2");
        assert_eq!(store.client_for(&locator).webhook_id(), "hook2");

        // Written when there was one webhook, so nothing was recorded; the
        // first client is the one that wrote it.
        let old = DiscordLocator::new(ObjectId::new(), "m".into(), "a".into(), "u".into(), 1);
        assert_eq!(store.client_for(&old).webhook_id(), "hook0");

        // A webhook that has since been removed falls back rather than
        // panicking; the read then fails on Discord's side, which is honest.
        let gone = DiscordLocator::new(ObjectId::new(), "m".into(), "a".into(), "u".into(), 1)
            .from_webhook("retired");
        assert_eq!(store.client_for(&gone).webhook_id(), "hook0");
    }

    /// The whole point: two webhooks, and each file's parts are all reachable
    /// through the one that holds them.
    #[tokio::test]
    async fn objects_round_trip_through_whichever_webhook_took_them() {
        let mut server = Server::new_async().await;
        let dl = format!("{}/dl", server.url());
        for hook in ["hook_a", "hook_b"] {
            server
                .mock("POST", format!("/webhooks/{hook}/tok").as_str())
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(format!(
                    r#"{{"id":"msg_{hook}","attachments":[{{"id":"att","filename":"o.bin","size":4,"url":"{dl}","proxy_url":"p"}}]}}"#
                ))
                .expect_at_least(0)
                .create_async()
                .await;
        }
        server
            .mock("GET", "/dl")
            .with_status(200)
            .with_body("keep")
            .expect_at_least(0)
            .create_async()
            .await;

        let clients = ["hook_a", "hook_b"]
            .iter()
            .map(|h| {
                DiscordClient::new(DiscordClientConfig::new(*h, "tok").with_base_url(server.url()))
            })
            .collect();
        let store =
            DiscordObjectStore::with_clients(clients, Arc::new(MemoryLocatorStore::default()));

        // One object per file, so the two files take whichever webhook they
        // hash to — between them both are exercised over enough files.
        for _ in 0..12 {
            let id = ObjectId::new();
            store
                .put_for(id, Bytes::from("keep"), Uuid::new_v4())
                .await
                .unwrap();
            assert_eq!(
                store.get(&ObjectLocator::new(id)).await.unwrap(),
                Bytes::from("keep"),
                "read back through the webhook that took it"
            );
        }
    }
}
