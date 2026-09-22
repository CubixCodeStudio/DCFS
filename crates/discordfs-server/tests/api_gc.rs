//! Garbage collection: deleting or overwriting a file eventually removes its
//! bytes from the object store, and never removes bytes something still needs.

mod common;

use axum::http::StatusCode;
use common::{key, name, send, send_bytes};
use discordfs_db::{MemoryMetadataRepository, MetadataRepository};
use discordfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use discordfs_server::{build_router, gc, AppState, TEST_CHUNK_SIZE};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// A router plus the backends behind it, so tests can look at the object store
/// directly rather than inferring from the API.
struct Fixture {
    router: axum::Router,
    repo: Arc<dyn MetadataRepository>,
    store: Arc<MemoryObjectStore>,
    root_id: String,
}

async fn fixture() -> Fixture {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store = Arc::new(MemoryObjectStore::new());
    let router = build_router(AppState::new(
        repo.clone(),
        store.clone(),
        [0x11; 32],
        TEST_CHUNK_SIZE,
    ));
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();
    Fixture {
        router,
        repo,
        store,
        root_id,
    }
}

impl Fixture {
    async fn create_file(&self, raw_name: &[u8]) -> String {
        let (status, body) = send(
            &self.router,
            "POST",
            "/api/v1/nodes",
            Some(json!({
                "parent_id": self.root_id, "name": name(raw_name), "kind": "File",
                "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    /// Write and close. A write builds one staging version, and while it is
    /// open its chunks are in use, so the collector leaves them alone — which
    /// is what a client that is still writing needs.
    async fn write(&self, file: &str, offset: u64, data: &[u8]) {
        let (status, _) = send_bytes(
            &self.router,
            "PUT",
            &format!("/api/v1/nodes/{file}/data?offset={offset}"),
            data.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = send_bytes(
            &self.router,
            "POST",
            &format!("/api/v1/nodes/{file}/sync"),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    async fn read_all(&self, file: &str) -> Vec<u8> {
        let (status, body) = send_bytes(
            &self.router,
            "GET",
            &format!("/api/v1/nodes/{file}/data"),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    /// Sweep everything that is dead, with no retention window.
    async fn sweep(&self) -> gc::GcReport {
        let store: Arc<dyn ObjectStore> = self.store.clone();
        gc::collect_once(&self.repo, &store, Duration::ZERO)
            .await
            .expect("gc")
    }
}

#[tokio::test]
async fn deleting_a_file_deletes_its_objects() {
    let fx = fixture().await;
    let file = fx.create_file(b"doomed.bin").await;
    // Three chunks at the test chunk size.
    fx.write(&file, 0, &vec![b'x'; (TEST_CHUNK_SIZE * 3) as usize])
        .await;
    assert_eq!(fx.store.len(), 3, "one object per chunk");

    // A sweep before the delete must leave everything alone.
    assert_eq!(fx.sweep().await.objects_deleted, 0);
    assert_eq!(fx.store.len(), 3);

    let (status, _) = send(&fx.router, "DELETE", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(fx.store.len(), 3, "unlink alone does not touch the bytes");

    let report = fx.sweep().await;
    assert_eq!(report.objects_deleted, 3);
    assert_eq!(fx.store.len(), 0, "the bytes are gone from the backend");

    // A second sweep finds nothing left to do.
    assert_eq!(fx.sweep().await.objects_deleted, 0);
}

#[tokio::test]
async fn overwriting_collects_only_the_chunks_that_changed() {
    let fx = fixture().await;
    let file = fx.create_file(b"edited.bin").await;
    fx.write(&file, 0, &vec![b'a'; (TEST_CHUNK_SIZE * 3) as usize])
        .await;
    assert_eq!(fx.store.len(), 3);

    // Touch chunk 0 only. Chunks 1 and 2 are carried over by reference.
    fx.write(&file, 0, b"z").await;
    assert_eq!(fx.store.len(), 4, "the rewritten chunk is a new object");

    let report = fx.sweep().await;
    assert_eq!(report.objects_deleted, 1, "only the superseded chunk goes");
    assert_eq!(fx.store.len(), 3);

    // And the file still reads correctly: the reused objects survived.
    let content = fx.read_all(&file).await;
    assert_eq!(content.len() as u64, TEST_CHUNK_SIZE * 3);
    assert_eq!(content[0], b'z');
    assert!(content[1..].iter().all(|&b| b == b'a'));
}

#[tokio::test]
async fn an_object_two_versions_share_survives_until_both_are_dead() {
    let fx = fixture().await;
    let file = fx.create_file(b"shared.bin").await;
    fx.write(&file, 0, &vec![b'a'; (TEST_CHUNK_SIZE * 2) as usize])
        .await;

    // v2 keeps chunk 1 by reference, so that object is in two versions at once.
    fx.write(&file, 0, b"z").await;
    let shared = fx.store.len();
    assert_eq!(shared, 3);

    // v1 is dead now, but the object it shares with v2 must not be collected.
    assert_eq!(fx.sweep().await.objects_deleted, 1);
    assert_eq!(fx.store.len(), 2);
    assert_eq!(fx.read_all(&file).await.len() as u64, TEST_CHUNK_SIZE * 2);

    // Once the file goes, both of its objects go.
    send(&fx.router, "DELETE", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(fx.sweep().await.objects_deleted, 2);
    assert_eq!(fx.store.len(), 0);
}

#[tokio::test]
async fn retention_holds_deleted_bytes_for_its_window() {
    let fx = fixture().await;
    let file = fx.create_file(b"held.bin").await;
    fx.write(&file, 0, b"bytes").await;
    send(&fx.router, "DELETE", &format!("/api/v1/nodes/{file}"), None).await;

    // Nothing that died in the last hour is collectable yet.
    let store: Arc<dyn ObjectStore> = fx.store.clone();
    let report = gc::collect_once(&fx.repo, &store, Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(report.objects_deleted, 0);
    assert_eq!(fx.store.len(), 1, "retention must hold the bytes");

    // With the window closed, they go.
    assert_eq!(fx.sweep().await.objects_deleted, 1);
    assert_eq!(fx.store.len(), 0);
}

#[tokio::test]
async fn a_live_file_is_never_collected() {
    let fx = fixture().await;
    let keep = fx.create_file(b"keep.bin").await;
    let drop = fx.create_file(b"drop.bin").await;
    fx.write(&keep, 0, b"keep these bytes").await;
    fx.write(&drop, 0, b"drop these bytes").await;
    assert_eq!(fx.store.len(), 2);

    send(&fx.router, "DELETE", &format!("/api/v1/nodes/{drop}"), None).await;
    assert_eq!(fx.sweep().await.objects_deleted, 1);

    assert_eq!(fx.store.len(), 1);
    assert_eq!(fx.read_all(&keep).await, b"keep these bytes");
}

#[tokio::test]
async fn an_abandoned_upload_is_eventually_collected() {
    let fx = fixture().await;
    let file = fx.create_file(b"abandoned.bin").await;

    // Stage a version and upload a chunk, then never commit: what a client
    // that died mid-upload leaves behind.
    let (status, staged) = send(
        &fx.router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": file, "expected_generation": 0,
            "expected_current_version": null, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{staged}");
    let vid = staged["version_id"].as_str().unwrap();
    let uri = format!(
        "/api/v1/versions/{vid}/chunks?chunk_index=0&plaintext_size=8&plaintext_hash=&idempotency_key={}",
        key()
    );
    let (status, _) = send_bytes(&fx.router, "POST", &uri, vec![b'x'; 8]).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(fx.store.len(), 1);

    // While the upload could still be in flight, nothing is touched.
    let store: Arc<dyn ObjectStore> = fx.store.clone();
    let held = gc::collect_once(&fx.repo, &store, Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        held.objects_deleted, 0,
        "an upload in progress must be safe"
    );
    assert_eq!(fx.store.len(), 1);

    // Past the window it is abandoned, and its bytes go.
    let report = fx.sweep().await;
    assert_eq!(report.objects_deleted, 1);
    assert_eq!(report.versions_purged, 1);
    assert_eq!(fx.store.len(), 0);
}

#[tokio::test]
async fn an_abandoned_write_gives_the_file_its_real_size_back() {
    let fx = fixture().await;
    let file = fx.create_file(b"half-written.bin").await;

    // A complete write, then a second one that is never closed.
    fx.write(&file, 0, b"committed").await;
    let (status, _) = send_bytes(
        &fx.router,
        "PUT",
        &format!("/api/v1/nodes/{file}/data?offset=9"),
        vec![b'x'; 200],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // While the write is open the size counts it, so `stat` tells the truth.
    let (_, node) = send(&fx.router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["size"], 209);

    // The writer disappears. Once the collector gives up on that version, the
    // file must not keep claiming bytes that were never committed — otherwise
    // it reads as zero-filled past its real end.
    let report = fx.sweep().await;
    assert!(report.objects_deleted >= 1, "the abandoned part goes");
    assert_eq!(report.sizes_corrected, 1);

    let (_, node) = send(&fx.router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["size"], 9, "back to what was committed");
    assert_eq!(fx.read_all(&file).await, b"committed");
}

/// An upload abandoned long enough for the collector to reach it must still
/// be resumable the next day: the file falls back to its last committed size,
/// and writing on from there leaves no hole where the collected parts were.
#[tokio::test]
async fn an_upload_resumed_after_the_collector_ran_has_no_hole() {
    let fx = fixture().await;
    let file = fx.create_file(b"resumed.bin").await;

    // A committed prefix, then more that is never closed.
    fx.write(&file, 0, b"committed-prefix").await;
    let (status, _) = send_bytes(
        &fx.router,
        "PUT",
        &format!("/api/v1/nodes/{file}/data?offset=16"),
        vec![b'x'; 64],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The writer is gone and the collector gives up on the open version.
    let report = fx.sweep().await;
    assert_eq!(report.sizes_corrected, 1);

    // A new client resumes from what the file now says it holds.
    let (_, node) = send(&fx.router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let resume_from = node["size"].as_u64().unwrap();
    assert_eq!(resume_from, 16, "back to the committed prefix");

    let (status, _) = send_bytes(
        &fx.router,
        "PUT",
        &format!("/api/v1/nodes/{file}/data?offset={resume_from}"),
        b"-and-the-rest".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_bytes(
        &fx.router,
        "POST",
        &format!("/api/v1/nodes/{file}/sync"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert_eq!(fx.read_all(&file).await, b"committed-prefix-and-the-rest");
}
