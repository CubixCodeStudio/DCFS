//! Reading part of a large file must fetch only the parts it covers.
//!
//! This is what makes the filesystem usable over a backend where every chunk
//! is a separate network round trip: seeking into the middle of a large file
//! must not pull the whole thing down.

mod common;

use async_trait::async_trait;
use axum::http::StatusCode;
use bytes::Bytes;
use common::{key, name, send, send_bytes};
use dcfs_core::ObjectId;
use dcfs_db::{MemoryMetadataRepository, MetadataRepository};
use dcfs_objectstore::{
    memory::MemoryObjectStore, ObjectLocator, ObjectStore, ObjectStoreError, StoredObject,
};
use dcfs_server::{build_router, AppState, TEST_CHUNK_SIZE};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Wraps a store and counts fetches, so a test can assert on what the server
/// actually pulled rather than on what it returned.
struct CountingStore {
    inner: MemoryObjectStore,
    gets: AtomicUsize,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: MemoryObjectStore::new(),
            gets: AtomicUsize::new(0),
        }
    }

    fn take_gets(&self) -> usize {
        self.gets.swap(0, Ordering::SeqCst)
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put(&self, id: ObjectId, data: Bytes) -> Result<StoredObject, ObjectStoreError> {
        self.inner.put(id, data).await
    }

    async fn get(&self, locator: &ObjectLocator) -> Result<Bytes, ObjectStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(locator).await
    }

    async fn delete(&self, locator: &ObjectLocator) -> Result<(), ObjectStoreError> {
        self.inner.delete(locator).await
    }

    async fn stat(&self, locator: &ObjectLocator) -> Result<StoredObject, ObjectStoreError> {
        self.inner.stat(locator).await
    }
}

const CS: u64 = TEST_CHUNK_SIZE;
/// Big enough that fetching everything would be obviously wrong.
const CHUNKS: u64 = 20;

async fn fixture() -> (axum::Router, String, Arc<CountingStore>) {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store = Arc::new(CountingStore::new());
    let router = build_router(AppState::new(repo, store.clone(), [0x21; 32], CS));

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let (status, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root["id"], "name": name(b"large.bin"), "kind": "File",
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    let file_id = file["id"].as_str().unwrap().to_string();

    // Distinct byte per position, so a wrong offset is visible in the content.
    let payload: Vec<u8> = (0..CS * CHUNKS).map(|i| (i % 251) as u8).collect();
    let (status, _) = send_bytes(
        &router,
        "PUT",
        &format!("/api/v1/nodes/{file_id}/data?offset=0"),
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    store.take_gets();
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
async fn a_read_inside_one_part_fetches_only_that_part() {
    let (router, file, store) = fixture().await;

    // Ten bytes in the middle of part 12 of 20.
    let offset = CS * 12 + 5;
    let got = read_range(&router, &file, offset, 10).await;

    assert_eq!(got, expected(offset, 10));
    assert_eq!(
        store.take_gets(),
        1,
        "one part read must not touch the other {} parts",
        CHUNKS - 1
    );
}

#[tokio::test]
async fn a_read_straddling_a_boundary_fetches_exactly_two_parts() {
    let (router, file, store) = fixture().await;

    let offset = CS * 7 - 3;
    let got = read_range(&router, &file, offset, 6).await;

    assert_eq!(got, expected(offset, 6));
    assert_eq!(store.take_gets(), 2);
}

#[tokio::test]
async fn the_cost_of_a_read_follows_its_length_not_the_file_size() {
    let (router, file, store) = fixture().await;

    // Three parts' worth, from a file of twenty.
    let offset = CS * 4;
    let got = read_range(&router, &file, offset, CS * 3).await;

    assert_eq!(got.len() as u64, CS * 3);
    assert_eq!(got, expected(offset, CS * 3));
    assert_eq!(store.take_gets(), 3);
}

#[tokio::test]
async fn reading_the_tail_does_not_read_the_head() {
    let (router, file, store) = fixture().await;

    let offset = CS * (CHUNKS - 1);
    let got = read_range(&router, &file, offset, CS).await;

    assert_eq!(got, expected(offset, CS));
    assert_eq!(store.take_gets(), 1);
}

#[tokio::test]
async fn reading_past_the_end_fetches_nothing() {
    let (router, file, store) = fixture().await;

    assert!(read_range(&router, &file, CS * CHUNKS, 100)
        .await
        .is_empty());
    assert_eq!(store.take_gets(), 0, "no part covers the range");
}

#[tokio::test]
async fn a_whole_file_read_still_works() {
    let (router, file, store) = fixture().await;

    let got = read_range(&router, &file, 0, CS * CHUNKS).await;
    assert_eq!(got, expected(0, CS * CHUNKS));
    assert_eq!(store.take_gets() as u64, CHUNKS);
}

#[tokio::test]
async fn editing_one_part_of_a_large_file_touches_only_that_part() {
    let (router, file, store) = fixture().await;

    // Writing into part 3 reads part 3 to modify it, and nothing else.
    let offset = CS * 3 + 1;
    let (status, _) = send_bytes(
        &router,
        "PUT",
        &format!("/api/v1/nodes/{file}/data?offset={offset}"),
        b"EDIT".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store.take_gets(),
        1,
        "an edit must not download the whole file"
    );

    // Neighbouring parts are unchanged.
    assert_eq!(read_range(&router, &file, offset, 4).await, b"EDIT");
    assert_eq!(
        read_range(&router, &file, CS * 2, 8).await,
        expected(CS * 2, 8)
    );
    assert_eq!(
        read_range(&router, &file, CS * 4, 8).await,
        expected(CS * 4, 8)
    );
}
