//! Losing the network for a moment must look like a slow filesystem, not a
//! broken one: an application writing to a mount has no way to retry for
//! itself, so the client does it.

use discordfs_core::RetryConfig;
use discordfs_fuse::{ClientError, HttpClient, ServerClient};
use discordfs_protocol::{CreateNodeRequest, NameBytes};
use mockito::Server;
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;

/// Short backoffs, so a test that exercises three attempts still finishes fast.
fn client(base: &str) -> HttpClient {
    HttpClient::new(base).with_retry(
        RetryConfig::new()
            .with_max_attempts(4)
            .with_initial_backoff(Duration::from_millis(10))
            .with_max_backoff(Duration::from_millis(40))
            .with_jitter(false),
    )
}

fn node_json(id: Uuid, name: &[u8]) -> String {
    json!({
        "id": id,
        "parent_id": Uuid::nil(),
        "name": NameBytes(name.to_vec()).to_base64(),
        "kind": "File",
        "mode": 0o100644,
        "uid": 0,
        "gid": 0,
        "size": 0,
        "atime": "2026-01-01T00:00:00Z",
        "mtime": "2026-01-01T00:00:00Z",
        "ctime": "2026-01-01T00:00:00Z",
        "current_version_id": null,
        "generation": 0
    })
    .to_string()
}

#[tokio::test]
async fn a_read_rides_out_a_server_that_is_briefly_unavailable() {
    let mut server = Server::new_async().await;
    let id = Uuid::new_v4();

    // Two failures, then the server comes back.
    let flaky = server
        .mock("GET", format!("/api/v1/nodes/{id}").as_str())
        .with_status(503)
        .expect(2)
        .create_async()
        .await;
    let recovered = server
        .mock("GET", format!("/api/v1/nodes/{id}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(node_json(id, b"back.txt"))
        .expect(1)
        .create_async()
        .await;

    let node = client(&server.url()).get_node(id).await.unwrap();
    assert_eq!(node.id, id);
    flaky.assert_async().await;
    recovered.assert_async().await;
}

#[tokio::test]
async fn a_write_rides_out_the_same_outage() {
    let mut server = Server::new_async().await;
    let id = Uuid::new_v4();
    let path = format!("/api/v1/nodes/{id}/data?offset=4096");

    let flaky = server
        .mock("PUT", path.as_str())
        .with_status(502)
        .expect(1)
        .create_async()
        .await;
    let recovered = server
        .mock("PUT", path.as_str())
        .with_status(200)
        .with_body("5")
        .expect(1)
        .create_async()
        .await;

    // Writing the same bytes at the same offset twice lands them in the same
    // place, which is what makes replaying a write safe.
    let written = client(&server.url())
        .write_file(id, 4096, b"hello")
        .await
        .unwrap();
    assert_eq!(written, 5);
    flaky.assert_async().await;
    recovered.assert_async().await;
}

#[tokio::test]
async fn a_create_whose_answer_was_lost_is_not_a_conflict() {
    let mut server = Server::new_async().await;
    let key = Uuid::new_v4();

    // The first attempt reached the server and worked; its answer did not come
    // back. The replay collides with the node the first attempt created.
    let failed = server
        .mock("POST", "/api/v1/nodes")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let conflict = server
        .mock("POST", "/api/v1/nodes")
        .with_status(409)
        .with_header("content-type", "application/json")
        .with_body(r#"{"code":"conflict","message":"resource already exists","request_id":null}"#)
        .expect(1)
        .create_async()
        .await;
    // The server uses the idempotency key as the new node's id, so asking for
    // it settles whether the collision is with ourselves.
    let lookup = server
        .mock("GET", format!("/api/v1/nodes/{key}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(node_json(key, b"mine.txt"))
        .expect(1)
        .create_async()
        .await;

    let created = client(&server.url())
        .create_node(CreateNodeRequest {
            parent_id: Uuid::nil(),
            name: NameBytes::new(b"mine.txt".to_vec()).unwrap(),
            kind: discordfs_core::NodeKind::File,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            link_target: None,
            idempotency_key: key,
        })
        .await
        .unwrap();
    assert_eq!(created.id, key);

    failed.assert_async().await;
    conflict.assert_async().await;
    lookup.assert_async().await;
}

#[tokio::test]
async fn a_create_that_really_collides_still_fails() {
    let mut server = Server::new_async().await;
    let key = Uuid::new_v4();

    // No retry happened, so a 409 means what it says.
    let conflict = server
        .mock("POST", "/api/v1/nodes")
        .with_status(409)
        .expect(1)
        .create_async()
        .await;

    let result = client(&server.url())
        .create_node(CreateNodeRequest {
            parent_id: Uuid::nil(),
            name: NameBytes::new(b"taken.txt".to_vec()).unwrap(),
            kind: discordfs_core::NodeKind::File,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            link_target: None,
            idempotency_key: key,
        })
        .await;
    assert!(matches!(result, Err(ClientError::AlreadyExists)));
    conflict.assert_async().await;
}

#[tokio::test]
async fn a_delete_replayed_after_it_worked_reports_success() {
    let mut server = Server::new_async().await;
    let id = Uuid::new_v4();
    let path = format!("/api/v1/nodes/{id}");

    let failed = server
        .mock("DELETE", path.as_str())
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    // The replay finds nothing left, which is the outcome that was asked for.
    let gone = server
        .mock("DELETE", path.as_str())
        .with_status(404)
        .expect(1)
        .create_async()
        .await;

    client(&server.url()).delete_node(id).await.unwrap();
    failed.assert_async().await;
    gone.assert_async().await;
}

#[tokio::test]
async fn an_outage_that_does_not_end_still_returns_an_error() {
    // Nothing listens here. Blocking a process forever is worse than telling
    // it the truth, so the retries are bounded.
    let started = std::time::Instant::now();
    let result = client("http://127.0.0.1:1").get_node(Uuid::new_v4()).await;
    let elapsed = started.elapsed();

    assert!(matches!(result, Err(ClientError::Http(_))));
    assert!(
        elapsed >= Duration::from_millis(50),
        "gave up without waiting"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "waited far too long: {elapsed:?}"
    );
}
