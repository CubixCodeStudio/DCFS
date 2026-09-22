//! A small read must cost a segment, not a whole part.
//!
//! Parts are sized for the backend's upload limit, so they are large — 16 MiB
//! in production. Sealing each part as one AEAD message meant a 4 KiB read had
//! to fetch and authenticate all 16 MiB of it. Sealing it as independently
//! authenticated segments makes the cost of a read follow its length.

mod common;

use async_trait::async_trait;
use axum::http::StatusCode;
use bytes::Bytes;
use common::{key, name, send, send_bytes};
use discordfs_core::ObjectId;
use discordfs_crypto::SEGMENT_SIZE;
use discordfs_db::{MemoryMetadataRepository, MetadataRepository};
use discordfs_objectstore::{
    memory::MemoryObjectStore, ObjectLocator, ObjectStore, ObjectStoreError, StoredObject,
};
use discordfs_server::{build_router, AppState};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts the bytes the server asks a backend for, which over a network is
/// what a read actually costs.
struct ByteCountingStore {
    inner: MemoryObjectStore,
    fetched: AtomicUsize,
}

#[async_trait]
impl ObjectStore for ByteCountingStore {
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError> {
        self.inner.put(id, data).await
    }

    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError> {
        let bytes = self.inner.get(locator).await?;
        self.fetched.fetch_add(bytes.len(), Ordering::SeqCst);
        Ok(bytes)
    }

    async fn get_range(
        &self,
        locator: &ObjectLocator,
        offset: u64,
        len: u64,
    ) -> Result<Bytes, ObjectStoreError> {
        self.fetched.fetch_add(len as usize, Ordering::SeqCst);
        self.inner.get_range(locator, offset, len).await
    }

    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError> {
        self.inner.delete(locator).await
    }

    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError> {
        self.inner.stat(locator).await
    }
}

/// Four segments to a part, so a read can land inside one.
const PART: u64 = (SEGMENT_SIZE * 4) as u64;
const PARTS: u64 = 2;

async fn fixture() -> (axum::Router, String, Arc<ByteCountingStore>) {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store = Arc::new(ByteCountingStore {
        inner: MemoryObjectStore::new(),
        fetched: AtomicUsize::new(0),
    });
    let router = build_router(AppState::new(repo, store.clone(), [0x21; 32], PART));

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let (status, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root["id"], "name": name(b"plot.bin"), "kind": "File",
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    let file_id = file["id"].as_str().unwrap().to_string();

    let payload: Vec<u8> = (0..PART * PARTS).map(|i| (i % 251) as u8).collect();
    let (status, _) = send_bytes(
        &router,
        "PUT",
        &format!("/api/v1/nodes/{file_id}/data?offset=0"),
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_bytes(
        &router,
        "POST",
        &format!("/api/v1/nodes/{file_id}/sync"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    store.fetched.store(0, Ordering::SeqCst);
    (router, file_id, store)
}

async fn read_range(router: &axum::Router, file: &str, offset: u64, size: u64) -> Vec<u8> {
    let (status, body) = send_bytes(
        router,
        "GET",
        &format!("/api/v1/nodes/{file}/data?offset={offset}&size={size}"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

fn expected(offset: u64, len: u64) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_small_read_fetches_one_segment_not_the_whole_part() {
    let (router, file, store) = fixture().await;

    // Well inside the second segment of the first part.
    let offset = SEGMENT_SIZE as u64 + 4096;
    assert_eq!(
        read_range(&router, &file, offset, 4096).await,
        expected(offset, 4096)
    );

    let fetched = store.fetched.load(Ordering::SeqCst) as u64;
    assert!(
        fetched <= SEGMENT_SIZE as u64 + 64,
        "fetched {fetched} for a 4 KiB read; a segment is {SEGMENT_SIZE}"
    );
    assert!(fetched < PART, "must not pull the whole part");
}

#[tokio::test]
async fn a_read_across_a_segment_boundary_fetches_both() {
    let (router, file, store) = fixture().await;

    let offset = SEGMENT_SIZE as u64 - 8;
    assert_eq!(
        read_range(&router, &file, offset, 16).await,
        expected(offset, 16)
    );

    let fetched = store.fetched.load(Ordering::SeqCst) as u64;
    assert!(
        fetched <= (SEGMENT_SIZE as u64 * 2) + 64,
        "fetched {fetched}, expected about two segments"
    );
}

#[tokio::test]
async fn the_bytes_are_right_wherever_the_read_lands() {
    let (router, file, _) = fixture().await;

    for (offset, len) in [
        (0u64, 32u64),
        (SEGMENT_SIZE as u64 - 1, 2),
        (PART - 1, 2),
        (PART, 16),
        (PART * PARTS - 16, 16),
        (SEGMENT_SIZE as u64 * 5 + 77, 1000),
    ] {
        assert_eq!(
            read_range(&router, &file, offset, len).await,
            expected(offset, len),
            "read {offset}..{}",
            offset + len
        );
    }
}

#[tokio::test]
async fn a_whole_file_read_still_returns_every_byte() {
    let (router, file, _) = fixture().await;
    let all = read_range(&router, &file, 0, PART * PARTS).await;
    assert_eq!(all.len() as u64, PART * PARTS);
    assert_eq!(all, expected(0, PART * PARTS));
}
