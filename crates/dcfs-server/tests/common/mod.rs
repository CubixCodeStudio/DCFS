#![allow(dead_code)] // each test binary uses a different subset of these helpers
//! Shared helpers for the server integration tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// Drive one request through the router and return status + parsed JSON body.
///
/// An empty body comes back as `Value::Null` so callers can ignore it.
pub async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => request
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// Drive one request whose body is raw bytes (chunk uploads, file writes).
pub async fn send_bytes(
    router: &Router,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/octet-stream")
        .body(Body::from(body))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// Filenames travel as unpadded base64url; encode them the same way the wire type does.
pub fn name(bytes: &[u8]) -> String {
    dcfs_protocol::NameBytes(bytes.to_vec()).to_base64()
}

/// Every mutation DTO carries an idempotency key; `create_node` also uses it as
/// the new node's id, so tests that need a known id pass their own.
pub fn key() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}
