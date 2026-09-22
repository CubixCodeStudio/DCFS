//! The object-store contract, against real Discord.
//!
//! Skipped unless `DISCORD_WEBHOOK_ID` and `DISCORD_WEBHOOK_TOKEN` are set.
//! This **posts real attachments** to the channel the webhook belongs to and
//! deletes them again; point it at a channel you do not mind writing to.
//!
//! ```bash
//! set -a; . ./.env.discord-test; set +a
//! cargo test -p dcfs-discord --test live_discord -- --test-threads=1 --nocapture
//! ```
//!
//! Single-threaded on purpose: the tests share one webhook's rate-limit
//! budget, and hammering it teaches nothing that running them in order does
//! not.

use bytes::Bytes;
use dcfs_core::ObjectId;
use dcfs_discord::{DiscordClient, DiscordClientConfig, DiscordObjectStore};
use dcfs_objectstore::{ObjectLocator, ObjectStore, ObjectStoreError};

/// Build a store against the real API, or `None` when no credentials are set.
fn live_store() -> Option<DiscordObjectStore> {
    let id = std::env::var("DISCORD_WEBHOOK_ID").ok()?;
    let token = std::env::var("DISCORD_WEBHOOK_TOKEN").ok()?;
    if id.trim().is_empty() || token.trim().is_empty() {
        return None;
    }
    Some(DiscordObjectStore::new(DiscordClient::new(
        DiscordClientConfig::new(id, token),
    )))
}

macro_rules! store_or_skip {
    () => {
        match live_store() {
            Some(store) => store,
            None => {
                eprintln!("skipped: DISCORD_WEBHOOK_ID / DISCORD_WEBHOOK_TOKEN not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn an_object_round_trips_through_discord() {
    let store = store_or_skip!();
    let id = ObjectId::new();

    // Bytes that are not text, since a chunk is ciphertext in real use.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let stored = store
        .put(id, Bytes::from(payload.clone()))
        .await
        .expect("upload");
    assert_eq!(stored.id, id);
    assert_eq!(stored.size, payload.len() as u64);

    let locator = ObjectLocator::new(id);
    assert_eq!(
        store.stat(&locator).await.unwrap().size,
        payload.len() as u64
    );

    let fetched = store.get(&locator).await.expect("download");
    assert_eq!(
        fetched,
        Bytes::from(payload),
        "what came back is what went up"
    );

    // Reading twice must work: the second read uses the cached URL.
    assert_eq!(store.get(&locator).await.unwrap().len(), 4096);

    store.delete(&locator).await.expect("delete");
    assert!(
        matches!(
            store.get(&locator).await,
            Err(ObjectStoreError::NotFound(_))
        ),
        "a deleted object must be gone"
    );
}

#[tokio::test]
async fn an_object_is_immutable() {
    let store = store_or_skip!();
    let id = ObjectId::new();

    store.put(id, Bytes::from_static(b"first")).await.unwrap();
    let again = store.put(id, Bytes::from_static(b"second")).await;
    assert!(
        matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
        "an object id must not be reusable"
    );

    store.delete(&ObjectLocator::new(id)).await.ok();
}

#[tokio::test]
async fn an_unknown_object_is_not_found() {
    let store = store_or_skip!();
    let missing = ObjectLocator::new(ObjectId::new());
    assert!(matches!(
        store.get(&missing).await,
        Err(ObjectStoreError::NotFound(_))
    ));
    // Deleting what was never there is the end state that was asked for.
    assert!(matches!(
        store.delete(&missing).await,
        Err(ObjectStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_chunk_sized_object_fits_the_upload_limit() {
    let store = store_or_skip!();
    // Whatever the deployment sets CHUNK_SIZE to, a sealed part is that plus
    // 40 bytes. This checks the shape of a real part against the real limit,
    // which is the thing no mock can tell you.
    let size: usize = std::env::var("DISCORD_TEST_PART_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024 * 1024);
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let id = ObjectId::new();
    match store.put(id, Bytes::from(payload.clone())).await {
        Ok(stored) => {
            assert_eq!(stored.size, size as u64);
            let fetched = store.get(&ObjectLocator::new(id)).await.unwrap();
            assert_eq!(fetched.len(), size);
            store.delete(&ObjectLocator::new(id)).await.ok();
        }
        Err(e) => panic!(
            "a {size}-byte part was refused: {e}. Lower CHUNK_SIZE, or raise \
             DISCORD_TEST_PART_BYTES only if the backend really accepts more."
        ),
    }
}
