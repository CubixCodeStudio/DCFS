//! The browser UI's way in: a token for a cookie, and the cookie for access.

mod common;

use axum::http::StatusCode;
use dcfs_db::{MemoryMetadataRepository, MetadataRepository};
use dcfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use dcfs_server::{build_router, AppState, TEST_CHUNK_SIZE};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "a-token-long-enough-to-pass";

async fn fixture() -> axum::Router {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    let state = AppState::new(repo, store, [0x42; 32], TEST_CHUNK_SIZE).with_api_token(TOKEN);
    build_router(state)
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: &str,
) -> (StatusCode, Vec<String>, String) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, cookies, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn the_page_is_served_without_a_token() {
    let router = fixture().await;
    let (status, _, body) = request(&router, "GET", "/ui", None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("DCFS"), "the page came back");
}

/// The credential goes back as a cookie a script cannot read, so nothing is
/// kept in browser storage for a shared machine or an injected script to take.
#[tokio::test]
async fn signing_in_returns_a_cookie_no_script_can_read() {
    let router = fixture().await;

    let (status, cookies, _) = request(
        &router,
        "POST",
        "/ui/login",
        None,
        &format!(r#"{{"token":"{TOKEN}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let cookie = cookies.first().expect("a session cookie was set");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(
        !cookie.contains(TOKEN),
        "the API token itself is never the cookie"
    );
}

#[tokio::test]
async fn a_wrong_token_gets_nothing() {
    let router = fixture().await;
    let (status, cookies, _) =
        request(&router, "POST", "/ui/login", None, r#"{"token":"wrong"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(cookies.is_empty(), "no cookie for a failed sign-in");
}

#[tokio::test]
async fn the_cookie_opens_the_api_and_signing_out_closes_it() {
    let router = fixture().await;

    // Without it, nothing.
    let (status, _, _) = request(&router, "GET", "/api/v1/nodes/root", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, cookies, _) = request(
        &router,
        "POST",
        "/ui/login",
        None,
        &format!(r#"{{"token":"{TOKEN}"}}"#),
    )
    .await;
    let session = cookies[0].split(';').next().unwrap().to_string();

    let (status, _, _) = request(&router, "GET", "/api/v1/nodes/root", Some(&session), "").await;
    assert_eq!(status, StatusCode::OK, "the cookie is enough");

    // Signing out revokes it server-side, so a copy of the cookie is no longer
    // a working credential.
    let (status, _, _) = request(&router, "POST", "/ui/logout", Some(&session), "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = request(&router, "GET", "/api/v1/nodes/root", Some(&session), "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old cookie is dead");
}

/// Every language table has the same keys. A key present in one and not the
/// other renders as a blank label in the language that lacks it, which nobody
/// notices until a user does.
#[test]
fn every_language_translates_every_string() {
    let page = include_str!("../src/ui/index.html");
    let start = page.find("const STRINGS = {").expect("the language tables");
    let tables = &page[start..];
    let end = tables.find("\n};").expect("the end of the tables");
    let tables = &tables[..end];

    // Split on the language headers — `  th: {`, `  en: {` — and collect the
    // keys each one defines.
    let mut languages: Vec<(String, std::collections::BTreeSet<String>)> = Vec::new();
    for line in tables.lines() {
        let trimmed = line.trim();
        if line.starts_with("  ") && !line.starts_with("    ") && trimmed.ends_with(": {") {
            languages.push((
                trimmed.trim_end_matches(": {").to_string(),
                Default::default(),
            ));
        } else if line.starts_with("    ") && !line.starts_with("      ") {
            if let Some((key, _)) = trimmed.split_once(':') {
                if let Some((_, keys)) = languages.last_mut() {
                    keys.insert(key.trim().to_string());
                }
            }
        }
    }

    assert!(languages.len() >= 2, "found {languages:?}");
    let (first_lang, first_keys) = &languages[0];
    assert!(
        first_keys.len() > 10,
        "{first_lang} looks empty: {first_keys:?}"
    );
    for (lang, keys) in &languages[1..] {
        let missing: Vec<_> = first_keys.difference(keys).collect();
        let extra: Vec<_> = keys.difference(first_keys).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{lang} differs from {first_lang}: missing {missing:?}, extra {extra:?}"
        );
    }
}
