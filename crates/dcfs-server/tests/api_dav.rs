//! The WebDAV gateway: a second door onto the same filesystem.

mod common;

use axum::http::StatusCode;
use common::{key, name, send};
use dcfs_db::{MemoryMetadataRepository, MetadataRepository};
use dcfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use dcfs_server::{build_router, AppState, TEST_CHUNK_SIZE};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

async fn fixture() -> axum::Router {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    build_router(AppState::new(repo, store, [0x42; 32], TEST_CHUNK_SIZE))
}

/// Sends a WebDAV request and returns the status and body.
async fn dav(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn a_file_written_over_dav_reads_back_over_dav() {
    let router = fixture().await;

    let (status, _) = dav(&router, "PUT", "/dav/hello.txt", b"over webdav".to_vec()).await;
    assert!(status.is_success(), "PUT returned {status}");

    let (status, body) = dav(&router, "GET", "/dav/hello.txt", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"over webdav".to_vec());
}

/// The point of a second door: what goes in one way comes out the other.
#[tokio::test]
async fn dav_and_the_byte_api_see_the_same_filesystem() {
    let router = fixture().await;

    dav(
        &router,
        "PUT",
        "/dav/shared.bin",
        b"written over dav".to_vec(),
    )
    .await;

    let (status, node) = send(
        &router,
        "GET",
        "/api/v1/nodes/resolve?path=/shared.bin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(node["size"], 16);

    let id = node["id"].as_str().unwrap();
    let (status, bytes) = common::send_bytes(
        &router,
        "GET",
        &format!("/api/v1/nodes/{id}/data"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"written over dav".to_vec());
}

#[tokio::test]
async fn a_file_written_through_the_api_is_visible_over_dav() {
    let router = fixture().await;

    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let (_, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root["id"], "name": name(b"from-api.txt"), "kind": "File",
            "mode": 0o100644, "uid": 0, "gid": 0, "idempotency_key": key(),
        })),
    )
    .await;
    let id = file["id"].as_str().unwrap();
    common::send_bytes(
        &router,
        "PUT",
        &format!("/api/v1/nodes/{id}/data?offset=0"),
        b"written over http".to_vec(),
    )
    .await;
    common::send_bytes(
        &router,
        "POST",
        &format!("/api/v1/nodes/{id}/sync"),
        Vec::new(),
    )
    .await;

    let (status, body) = dav(&router, "GET", "/dav/from-api.txt", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"written over http".to_vec());
}

#[tokio::test]
async fn a_directory_lists_what_is_in_it() {
    let router = fixture().await;

    let (status, _) = dav(&router, "MKCOL", "/dav/notes", Vec::new()).await;
    assert!(status.is_success(), "MKCOL returned {status}");
    dav(&router, "PUT", "/dav/notes/one.txt", b"1".to_vec()).await;
    dav(&router, "PUT", "/dav/notes/two.txt", b"2".to_vec()).await;

    let (status, body) = dav(&router, "PROPFIND", "/dav/notes", Vec::new()).await;
    assert_eq!(status.as_u16(), 207, "PROPFIND answers Multi-Status");
    let xml = String::from_utf8_lossy(&body);
    assert!(xml.contains("one.txt"), "{xml}");
    assert!(xml.contains("two.txt"), "{xml}");
}

/// A PUT over an existing file replaces it. Without truncation the old tail
/// would still be there, and a file edited to something shorter would read
/// back with the previous contents hanging off the end.
#[tokio::test]
async fn writing_over_a_file_replaces_it_rather_than_merging() {
    let router = fixture().await;

    dav(
        &router,
        "PUT",
        "/dav/notes.txt",
        b"a much longer first version".to_vec(),
    )
    .await;
    dav(&router, "PUT", "/dav/notes.txt", b"short".to_vec()).await;

    let (_, body) = dav(&router, "GET", "/dav/notes.txt", Vec::new()).await;
    assert_eq!(body, b"short".to_vec());
}

#[tokio::test]
async fn deleting_and_moving_work() {
    let router = fixture().await;

    dav(&router, "PUT", "/dav/gone.txt", b"x".to_vec()).await;
    let (status, _) = dav(&router, "DELETE", "/dav/gone.txt", Vec::new()).await;
    assert!(status.is_success(), "DELETE returned {status}");
    let (status, _) = dav(&router, "GET", "/dav/gone.txt", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    dav(&router, "PUT", "/dav/before.txt", b"same bytes".to_vec()).await;
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("MOVE")
                .uri("/dav/before.txt")
                .header("destination", "/dav/after.txt")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "MOVE returned {}",
        response.status()
    );

    let (status, body) = dav(&router, "GET", "/dav/after.txt", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"same bytes".to_vec());
}

/// Desktop clients speak Basic, not Bearer. If the token were only accepted
/// one way, nothing could mount this.
#[tokio::test]
async fn the_token_is_accepted_as_a_basic_password() {
    use dcfs_server::auth;
    assert_eq!(
        auth::presented_token_for_test("Basic dXNlcjpzM2NyZXQ="),
        Some("s3cret".to_string()),
        "username ignored, password is the token"
    );
    assert_eq!(
        auth::presented_token_for_test("Bearer s3cret"),
        Some("s3cret".to_string())
    );
    assert_eq!(auth::presented_token_for_test("Digest nope"), None);
}
