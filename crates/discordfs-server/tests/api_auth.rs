//! Bearer-token enforcement.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use discordfs_db::{MemoryMetadataRepository, MetadataRepository};
use discordfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use discordfs_server::{build_router, AppState, TEST_CHUNK_SIZE};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "a-sufficiently-long-test-token";

fn guarded_router() -> axum::Router {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    build_router(AppState::new(repo, store, [7; 32], TEST_CHUNK_SIZE).with_api_token(TOKEN))
}

async fn status_with_auth(uri: &str, authorization: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    let response = guarded_router()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn health_needs_no_token() {
    // Probes run before anyone has a credential.
    for uri in ["/health", "/health/ready"] {
        let (status, _) = status_with_auth(uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn the_api_rejects_a_missing_or_wrong_token() {
    let cases = [
        (None, "no header"),
        (Some("Bearer "), "empty token"),
        (Some("Bearer wrong-token-entirely"), "wrong token"),
        (Some(TOKEN), "token without the Bearer prefix"),
        (Some("Basic YWRtaW46YWRtaW4="), "wrong scheme"),
        // A prefix of the real token must not pass.
        (Some("Bearer a-sufficiently-long"), "truncated token"),
    ];

    for (authorization, case) in cases {
        let (status, body) = status_with_auth("/api/v1/nodes/root", authorization).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{case}");
        assert!(body.contains("unauthorized"), "{case}: {body}");
        assert!(
            !body.contains(TOKEN),
            "{case}: the response must not echo the expected token"
        );
    }
}

#[tokio::test]
async fn the_api_accepts_the_configured_token() {
    let (status, body) =
        status_with_auth("/api/v1/nodes/root", Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn every_api_route_is_behind_the_token() {
    // Unauthenticated callers must not even learn whether a node exists.
    let id = uuid::Uuid::new_v4();
    let routes = [
        "/api/v1/nodes/root".to_string(),
        format!("/api/v1/nodes/{id}"),
        format!("/api/v1/nodes/{id}/children"),
        format!("/api/v1/nodes/{id}/data"),
        "/api/v1/nodes/resolve?path=/".to_string(),
        format!("/api/v1/versions/{id}"),
        format!("/api/v1/versions/{id}/chunks"),
        format!("/api/v1/objects/{id}"),
    ];
    for uri in routes {
        let (status, _) = status_with_auth(&uri, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{uri} must require a token"
        );
    }
}

// --- sessions --------------------------------------------------------------

use serde_json::{json, Value};

/// Drive a request with an arbitrary method, body and credential.
async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_session_token_works_and_can_be_revoked() {
    let router = guarded_router();

    let (status, issued) = request(
        &router,
        "POST",
        "/api/v1/sessions",
        Some(TOKEN),
        Some(json!({ "ttl_secs": 3600, "label": "laptop" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let session_token = issued["token"].as_str().unwrap().to_string();
    let session_id = issued["id"].as_str().unwrap().to_string();
    assert_ne!(session_token, TOKEN, "a session is not the bootstrap token");
    assert!(session_token.len() >= 64, "32 bytes, hex encoded");

    // It opens the API like the bootstrap token does.
    let (status, _) = request(
        &router,
        "GET",
        "/api/v1/nodes/root",
        Some(&session_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Revoking it closes that door immediately.
    let (status, _) = request(
        &router,
        "DELETE",
        &format!("/api/v1/sessions/{session_id}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = request(
        &router,
        "GET",
        "/api/v1/nodes/root",
        Some(&session_token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked session is dead"
    );
}

#[tokio::test]
async fn a_session_cannot_mint_or_revoke_sessions() {
    let router = guarded_router();
    let (_, issued) = request(
        &router,
        "POST",
        "/api/v1/sessions",
        Some(TOKEN),
        Some(json!({})),
    )
    .await;
    let session_token = issued["token"].as_str().unwrap().to_string();
    let session_id = issued["id"].as_str().unwrap().to_string();

    // Otherwise a leaked session renews itself forever.
    let (status, body) = request(
        &router,
        "POST",
        "/api/v1/sessions",
        Some(&session_token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, _) = request(
        &router,
        "DELETE",
        &format!("/api/v1/sessions/{session_id}"),
        Some(&session_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_expired_session_is_not_accepted() {
    let router = guarded_router();
    let (status, issued) = request(
        &router,
        "POST",
        "/api/v1/sessions",
        Some(TOKEN),
        // The handler clamps to at least one second; ask for the shortest.
        Some(json!({ "ttl_secs": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session_token = issued["token"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, _) = request(
        &router,
        "GET",
        "/api/v1/nodes/root",
        Some(&session_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sessions_need_the_bootstrap_token_to_issue() {
    let router = guarded_router();
    for bearer in [None, Some("not-the-token-at-all-really")] {
        let (status, _) =
            request(&router, "POST", "/api/v1/sessions", bearer, Some(json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
