//! Version staging, chunk upload, and atomic commit tests.

mod common;

use axum::http::StatusCode;
use common::{key, name, send, send_bytes};
use dcfs_server::create_server;
use serde_json::{json, Value};
use uuid::Uuid;

/// Router plus the id of one empty file, which every test needs.
async fn fixture() -> (axum::Router, String) {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    let (status, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root_id,
            "name": name(b"data.bin"),
            "kind": "File",
            "mode": 0o100644,
            "uid": 1000,
            "gid": 1000,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    (router, file["id"].as_str().unwrap().to_string())
}

async fn stage(
    router: &axum::Router,
    node_id: &str,
    generation: u64,
    current: Option<&str>,
) -> Value {
    let (status, body) = send(
        router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": node_id,
            "expected_generation": generation,
            "expected_current_version": current,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

/// Upload one chunk: metadata in the query string, plaintext in the body.
async fn upload(router: &axum::Router, vid: &str, index: u64, data: &[u8]) -> (StatusCode, Value) {
    let uri = format!(
        "/api/v1/versions/{vid}/chunks?chunk_index={index}&plaintext_size={}&plaintext_hash=&idempotency_key={}",
        data.len(),
        key()
    );
    let (status, body) = send_bytes(router, "POST", &uri, data.to_vec()).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Commit a staged version, asserting nothing so callers can check the status.
async fn commit(
    router: &axum::Router,
    vid: &str,
    total_size: u64,
    generation: u64,
    current: Option<&str>,
) -> (StatusCode, Value) {
    send(
        router,
        "POST",
        &format!("/api/v1/versions/{vid}/commit"),
        Some(json!({
            "version_id": vid,
            "total_size": total_size,
            "plaintext_hash": "total",
            "expected_generation": generation,
            "expected_current_version": current,
            "idempotency_key": key(),
        })),
    )
    .await
}

#[tokio::test]
async fn staging_bumps_the_generation_and_returns_a_chunk_size() {
    let (router, file) = fixture().await;

    let staged = stage(&router, &file, 0, None).await;
    assert_eq!(staged["node_id"], file);
    assert_eq!(staged["generation"], 1);
    assert!(staged["chunk_size"].as_u64().unwrap() > 0);

    // The node's generation moved, but the file is still empty until commit.
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["generation"], 1);
    assert!(node["current_version_id"].is_null());
    assert_eq!(node["size"], 0);
}

#[tokio::test]
async fn staging_an_unknown_node_is_not_found() {
    let router = create_server();
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/versions/stage",
        Some(json!({
            "node_id": Uuid::new_v4(),
            "expected_generation": 0,
            "expected_current_version": null,
            "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn multi_chunk_manifest_round_trips_in_index_order() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap();

    // Upload out of order to prove the manifest is sorted by index, not arrival.
    for index in [2u64, 0, 1] {
        let (status, body) = upload(&router, vid, index, &[index as u8; 8]).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(body["object_id"].as_str().is_some());
    }

    let (status, manifest) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{vid}/chunks"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = manifest.as_array().unwrap();
    assert_eq!(entries.len(), 3);
    let indexes: Vec<u64> = entries
        .iter()
        .map(|e| e["chunk_index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        indexes,
        vec![0, 1, 2],
        "manifest must be ordered by chunk index"
    );
    assert_eq!(entries[1]["plaintext_size"], 8);
}

#[tokio::test]
async fn re_uploading_a_chunk_index_replaces_it() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap();

    let mut object_ids = Vec::new();
    for attempt in 0..2u8 {
        let (status, body) = upload(&router, vid, 0, &[attempt; 8]).await;
        assert_eq!(status, StatusCode::CREATED);
        object_ids.push(body["object_id"].as_str().unwrap().to_string());
    }

    // A retried upload must leave one entry, not two.
    let (_, manifest) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{vid}/chunks"),
        None,
    )
    .await;
    let entries = manifest.as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "retry must not duplicate the chunk: {manifest}"
    );
    assert_eq!(
        entries[0]["object_id"], object_ids[1],
        "the newest object wins"
    );
    assert_ne!(
        object_ids[0], object_ids[1],
        "objects are immutable, so a retry writes a new one"
    );
}

#[tokio::test]
async fn uploading_to_an_unknown_version_is_not_found() {
    let router = create_server();
    let (status, _) = upload(&router, &Uuid::new_v4().to_string(), 0, b"x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn uploaded_chunk_bytes_are_stored_encrypted() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap();

    let plaintext = b"secret payload";
    let (status, uploaded) = upload(&router, vid, 0, plaintext).await;
    assert_eq!(status, StatusCode::CREATED);
    let object_id = uploaded["object_id"].as_str().unwrap();

    // The object exists, and what it holds is ciphertext, not the plaintext.
    let (status, stored) = send_bytes(
        &router,
        "GET",
        &format!("/api/v1/objects/{object_id}"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !stored.windows(plaintext.len()).any(|w| w == plaintext),
        "plaintext must not appear in the stored object"
    );
    // An 8-byte header plus one Poly1305 tag per segment.
    assert_eq!(
        stored.len() as u64,
        plaintext.len() as u64 + dcfs_crypto::ciphertext_overhead(plaintext.len() as u64)
    );
    assert_eq!(uploaded["ciphertext_size"], stored.len());
}

#[tokio::test]
async fn upload_rejects_claims_that_do_not_match_the_body() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap();

    let uri = format!(
        "/api/v1/versions/{vid}/chunks?chunk_index=0&plaintext_size=99&plaintext_hash=&idempotency_key={}",
        key()
    );
    let (status, _) = send_bytes(&router, "POST", &uri, b"short".to_vec()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a lying plaintext_size must be rejected"
    );

    let uri = format!(
        "/api/v1/versions/{vid}/chunks?chunk_index=0&plaintext_size=5&plaintext_hash=deadbeef&idempotency_key={}",
        key()
    );
    let (status, _) = send_bytes(&router, "POST", &uri, b"short".to_vec()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a wrong plaintext_hash must be rejected"
    );
}

#[tokio::test]
async fn commit_makes_the_version_visible_atomically() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap().to_string();

    let (status, _) = upload(&router, &vid, 0, b"hello world!").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = commit(&router, &vid, 12, 1, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version_id"], vid);
    assert_eq!(body["node_id"], file);

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["current_version_id"], vid);
    assert_eq!(node["size"], 12);

    let (status, fetched) = send(&router, "GET", &format!("/api/v1/versions/{vid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["version_id"], vid);
}

#[tokio::test]
async fn commit_rejects_a_manifest_with_a_hole() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap().to_string();

    // Chunk 0 is missing, so the version cannot describe a contiguous file.
    upload(&router, &vid, 1, &[b'x'; 8]).await;

    let (status, body) = commit(&router, &vid, 16, 1, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Nothing was half-committed.
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert!(node["current_version_id"].is_null());
}

#[tokio::test]
async fn commit_rejects_a_total_size_the_manifest_does_not_cover() {
    let (router, file) = fixture().await;
    let version = stage(&router, &file, 0, None).await;
    let vid = version["version_id"].as_str().unwrap().to_string();
    upload(&router, &vid, 0, &[b'x'; 8]).await;

    let (status, _) = commit(&router, &vid, 1_000, 1, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_stale_guard_conflicts_and_leaves_the_committed_bytes_visible() {
    let (router, file) = fixture().await;

    // Writer A stages and commits.
    let first = stage(&router, &file, 0, None).await;
    let v1 = first["version_id"].as_str().unwrap().to_string();
    upload(&router, &v1, 0, &[b'a'; 10]).await;
    let (status, _) = commit(&router, &v1, 10, 1, None).await;
    assert_eq!(status, StatusCode::OK);

    // Writer B staged against the pre-commit state and commits with a stale guard.
    let second = stage(&router, &file, 1, None).await;
    let v2 = second["version_id"].as_str().unwrap().to_string();
    upload(&router, &v2, 0, &[b'b'; 20]).await;
    let (status, body) = commit(&router, &v2, 20, 1, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");

    // The first writer's bytes are still the visible ones.
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["current_version_id"], v1);
    assert_eq!(node["size"], 10);
}

#[tokio::test]
async fn a_fresh_guard_supersedes_the_previous_version() {
    let (router, file) = fixture().await;

    let first = stage(&router, &file, 0, None).await;
    let v1 = first["version_id"].as_str().unwrap().to_string();
    upload(&router, &v1, 0, &[b'a'; 10]).await;
    commit(&router, &v1, 10, 1, None).await;

    let second = stage(&router, &file, 1, Some(&v1)).await;
    let v2 = second["version_id"].as_str().unwrap().to_string();
    upload(&router, &v2, 0, &[b'b'; 20]).await;
    let (status, _) = commit(&router, &v2, 20, 2, Some(&v1)).await;
    assert_eq!(status, StatusCode::OK);

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["current_version_id"], v2);
    assert_eq!(node["size"], 20);
}

#[tokio::test]
async fn committing_an_unknown_version_is_not_found() {
    let router = create_server();
    let (status, _) = commit(&router, &Uuid::new_v4().to_string(), 0, 0, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fetching_an_unknown_object_is_not_found() {
    let router = create_server();
    let (status, _) = send(
        &router,
        "GET",
        &format!("/api/v1/objects/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
