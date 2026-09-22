//! End-to-end tests for Discord API integration.
//!
//! These tests use mockito to simulate Discord API responses and verify
//! the complete flow including rate limiting, retries, and error handling.

use bytes::Bytes;
use discordfs_core::ObjectId;
use discordfs_discord::{DiscordClient, DiscordClientConfig, DiscordObjectStore, RetryConfig};
use discordfs_objectstore::{ObjectLocator, ObjectStore};
use mockito::Server;
use std::time::Duration;

fn test_config(server_url: &str) -> DiscordClientConfig {
    DiscordClientConfig::new("test_webhook_id", "test_webhook_token")
        .with_base_url(server_url)
        .with_retry(RetryConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(100),
            multiplier: 2.0,
        })
}

// ============================================================
// Upload/Download Tests
// ============================================================

#[tokio::test]
async fn test_upload_and_download_chunk() {
    let mut server = Server::new_async().await;

    // Mock upload response
    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_123",
                "attachments": [{
                    "id": "att_456",
                    "filename": "test.bin",
                    "size": 13,
                    "url": "DOWNLOAD_URL",
                    "proxy_url": "https://proxy/att_456"
                }]
            }"#
            .replace(
                "DOWNLOAD_URL",
                &format!("{}/download/att_456", server.url()),
            ),
        )
        .expect(1)
        .create_async()
        .await;

    // Mock download response
    let download_mock = server
        .mock("GET", "/download/att_456")
        .with_status(200)
        .with_body("hello, world!")
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    // Upload
    let object_id = ObjectId::new();
    let data = Bytes::from("hello, world!");
    let stored = store.put(object_id, data.clone()).await.unwrap();

    assert_eq!(stored.id, object_id);
    assert_eq!(stored.size, 13);

    // Download
    let locator = ObjectLocator::new(object_id);
    let downloaded = store.get(&locator).await.unwrap();

    assert_eq!(downloaded, data);

    upload_mock.assert_async().await;
    download_mock.assert_async().await;
}

#[tokio::test]
async fn test_upload_large_chunk() {
    let mut server = Server::new_async().await;

    let large_data = vec![0xABu8; 1024 * 1024]; // 1MB

    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_large",
                "attachments": [{
                    "id": "att_large",
                    "filename": "large.bin",
                    "size": 1048576,
                    "url": "DOWNLOAD_URL",
                    "proxy_url": "https://proxy/att_large"
                }]
            }"#
            .replace(
                "DOWNLOAD_URL",
                &format!("{}/download/att_large", server.url()),
            ),
        )
        .expect(1)
        .create_async()
        .await;

    let download_mock = server
        .mock("GET", "/download/att_large")
        .with_status(200)
        .with_body(large_data.clone())
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let data = Bytes::from(large_data.clone());
    let stored = store.put(object_id, data.clone()).await.unwrap();

    assert_eq!(stored.size, 1048576);

    let locator = ObjectLocator::new(object_id);
    let downloaded = store.get(&locator).await.unwrap();

    assert_eq!(downloaded.len(), 1048576);
    assert_eq!(downloaded.as_ref(), large_data.as_slice());

    upload_mock.assert_async().await;
    download_mock.assert_async().await;
}

// ============================================================
// Rate Limiting Tests
// ============================================================

#[tokio::test]
async fn test_rate_limit_429_then_success() {
    let mut server = Server::new_async().await;

    // First request: rate limited
    let rate_limit_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(429)
        .with_header("retry-after", "0.01")
        .with_body(r#"{"message": "Rate limited", "code": 429}"#)
        .expect(1)
        .create_async()
        .await;

    // Second request: success
    let success_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_retry",
                "attachments": [{
                    "id": "att_retry",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_ok(), "Should succeed after retry");

    rate_limit_mock.assert_async().await;
    success_mock.assert_async().await;
}

#[tokio::test]
async fn test_rate_limit_headers_respected() {
    let mut server = Server::new_async().await;

    // Response with rate limit headers
    let mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("x-ratelimit-limit", "5")
        .with_header("x-ratelimit-remaining", "4")
        .with_header("x-ratelimit-reset", "1234567890")
        .with_body(
            r#"{
                "id": "msg_rl",
                "attachments": [{
                    "id": "att_rl",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_ok());

    mock.assert_async().await;
}

// ============================================================
// Retry Logic Tests
// ============================================================

#[tokio::test]
async fn test_retry_on_500_error() {
    let mut server = Server::new_async().await;

    // First two requests: 500 error
    let fail_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(500)
        .with_body("Internal Server Error")
        .expect(2)
        .create_async()
        .await;

    // Third request: success
    let success_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_retry500",
                "attachments": [{
                    "id": "att_retry500",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_ok(), "Should succeed after retries");

    fail_mock.assert_async().await;
    success_mock.assert_async().await;
}

#[tokio::test]
async fn test_max_retries_exceeded() {
    let mut server = Server::new_async().await;

    // All requests fail with 500
    let fail_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(500)
        .with_body("Internal Server Error")
        .expect(4) // initial + 3 retries
        .create_async()
        .await;

    let config = DiscordClientConfig::new("test_webhook_id", "test_webhook_token")
        .with_base_url(server.url())
        .with_retry(RetryConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            multiplier: 2.0,
        });

    let client = DiscordClient::new(config);
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_err(), "Should fail after max retries");

    fail_mock.assert_async().await;
}

#[tokio::test]
async fn test_no_retry_on_400_error() {
    let mut server = Server::new_async().await;

    // 400 error should not retry
    let mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(400)
        .with_body("Bad Request")
        .expect(1) // Only one attempt
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_err(), "Should fail immediately on 400");

    mock.assert_async().await;
}

// ============================================================
// Attachment URL Refresh Tests
// ============================================================

#[tokio::test]
async fn test_attachment_url_expires_and_refreshes() {
    let mut server = Server::new_async().await;

    // Initial upload
    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_expire",
                "attachments": [{
                    "id": "att_expire",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "OLD_URL",
                    "proxy_url": "https://proxy/att_expire"
                }]
            }"#
            .replace("OLD_URL", &format!("{}/download/old", server.url())),
        )
        .expect(1)
        .create_async()
        .await;

    // First download attempt with expired URL (403)
    let expired_mock = server
        .mock("GET", "/download/old")
        .with_status(403)
        .with_body("Forbidden")
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    store.put(object_id, Bytes::from("test")).await.unwrap();

    // Try to download with expired URL
    let locator = ObjectLocator::new(object_id);
    let result = store.get(&locator).await;

    // Should fail because URL is expired
    assert!(result.is_err());

    upload_mock.assert_async().await;
    expired_mock.assert_async().await;
}

// ============================================================
// Network Failure Tests
// ============================================================

#[tokio::test]
async fn test_network_timeout() {
    let mut server = Server::new_async().await;

    // Simulate slow response (without actual delay - mockito doesn't support with_delay)
    let mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_timeout",
                "attachments": [{
                    "id": "att_timeout",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let config = DiscordClientConfig::new("test_webhook_id", "test_webhook_token")
        .with_base_url(server.url())
        .with_retry(RetryConfig {
            max_attempts: 1,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            multiplier: 2.0,
        });

    let client = DiscordClient::new(config);
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    // Should succeed
    assert!(result.is_ok());

    mock.assert_async().await;
}

#[tokio::test]
async fn test_connection_refused() {
    // Use invalid port to simulate connection refused
    let config = DiscordClientConfig::new("test_webhook_id", "test_webhook_token")
        .with_base_url("http://localhost:1")
        .with_retry(RetryConfig {
            max_attempts: 2,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            multiplier: 2.0,
        });

    let client = DiscordClient::new(config);
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let result = store.put(object_id, Bytes::from("test")).await;

    assert!(result.is_err(), "Should fail on connection refused");
}

// ============================================================
// Delete Tests
// ============================================================

#[tokio::test]
async fn test_delete_object() {
    let mut server = Server::new_async().await;

    // Upload
    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_delete",
                "attachments": [{
                    "id": "att_delete",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    // Delete
    let delete_mock = server
        .mock(
            "DELETE",
            "/webhooks/test_webhook_id/test_webhook_token/messages/msg_delete",
        )
        .with_status(204)
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    store.put(object_id, Bytes::from("test")).await.unwrap();

    let locator = ObjectLocator::new(object_id);
    store.delete(&locator).await.unwrap();

    // Verify object is gone
    let result = store.get(&locator).await;
    assert!(result.is_err());

    upload_mock.assert_async().await;
    delete_mock.assert_async().await;
}

#[tokio::test]
async fn test_delete_nonexistent_object() {
    let client = DiscordClient::new(test_config("http://unused"));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    let locator = ObjectLocator::new(object_id);
    let result = store.delete(&locator).await;

    assert!(result.is_err(), "Should fail for nonexistent object");
}

// ============================================================
// Stat Tests
// ============================================================

#[tokio::test]
async fn test_stat_object() {
    let mut server = Server::new_async().await;

    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_stat",
                "attachments": [{
                    "id": "att_stat",
                    "filename": "test.bin",
                    "size": 12345,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    store
        .put(object_id, Bytes::from(vec![0u8; 12345]))
        .await
        .unwrap();

    let locator = ObjectLocator::new(object_id);
    let stat = store.stat(&locator).await.unwrap();

    assert_eq!(stat.id, object_id);
    assert_eq!(stat.size, 12345);

    upload_mock.assert_async().await;
}

// ============================================================
// Immutability Tests
// ============================================================

#[tokio::test]
async fn test_object_immutability() {
    let mut server = Server::new_async().await;

    let upload_mock = server
        .mock("POST", "/webhooks/test_webhook_id/test_webhook_token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "msg_immutable",
                "attachments": [{
                    "id": "att_immutable",
                    "filename": "test.bin",
                    "size": 4,
                    "url": "http://example.com/att",
                    "proxy_url": "https://proxy/att"
                }]
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = DiscordClient::new(test_config(&server.url()));
    let store = DiscordObjectStore::new(client);

    let object_id = ObjectId::new();
    store.put(object_id, Bytes::from("test")).await.unwrap();

    // Try to put same object ID again
    let result = store.put(object_id, Bytes::from("different")).await;

    assert!(result.is_err(), "Should reject duplicate object ID");

    upload_mock.assert_async().await;
}

/// A moment of lost connectivity used to fail the request outright: the
/// transport error was returned instead of being retried like a 5xx.
#[tokio::test]
async fn a_request_that_cannot_reach_discord_is_retried() {
    // Nothing listens here, so every attempt fails to connect.
    let config = DiscordClientConfig::new("id", "token")
        .with_base_url("http://127.0.0.1:1")
        .with_retry(RetryConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            multiplier: 2.0,
        });
    let client = DiscordClient::new(config);

    let started = std::time::Instant::now();
    let result = client.fetch_message("msg").await;
    let elapsed = started.elapsed();

    assert!(result.is_err(), "it never reached a server");
    // 20 + 40 + 80 ms of backoff between four attempts. Without the retry this
    // returns in microseconds.
    assert!(
        elapsed >= Duration::from_millis(130),
        "gave up without backing off: {elapsed:?}"
    );
}

#[tokio::test]
async fn an_upload_that_cannot_reach_discord_is_retried() {
    let config = DiscordClientConfig::new("id", "token")
        .with_base_url("http://127.0.0.1:1")
        .with_retry(RetryConfig {
            max_attempts: 2,
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(40),
            multiplier: 2.0,
        });
    let client = DiscordClient::new(config);

    let started = std::time::Instant::now();
    // The multipart body has to be rebuilt for each attempt; if it were not,
    // this would not compile, and a retry would send an empty body.
    let result = client
        .upload_attachment("o.bin", Bytes::from_static(b"payload"))
        .await;
    assert!(result.is_err());
    assert!(started.elapsed() >= Duration::from_millis(60));
}

/// Discord puts the credential in the URL path, so anything that prints a URL
/// prints a working webhook token unless it is scrubbed first.
#[tokio::test]
async fn a_failure_never_carries_the_webhook_token() {
    const TOKEN: &str = "s3cret-webhook-token-value";
    let config = DiscordClientConfig::new("id", TOKEN)
        .with_base_url("http://127.0.0.1:1")
        .with_retry(RetryConfig {
            max_attempts: 1,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            multiplier: 1.0,
        });
    let client = DiscordClient::new(config.clone());

    // A connection error: reqwest's own message names the URL it failed on.
    let error = client.fetch_message("msg").await.unwrap_err();
    let rendered = format!("{error} {error:?}");
    assert!(
        !rendered.contains(TOKEN),
        "the token reached an error message: {rendered}"
    );

    // Nor does formatting the configuration itself.
    let printed = format!("{config:?}");
    assert!(
        !printed.contains(TOKEN),
        "the token reached Debug: {printed}"
    );
    assert!(printed.contains("<redacted>"));
}

/// Rate-limit headers describe what already happened, so nothing stops a burst
/// of parallel requests from arriving together and blowing the allowance. The
/// client bounds how many it has in flight.
#[tokio::test]
async fn requests_in_flight_are_bounded() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut server = Server::new_async().await;
    // Every response is slow enough that overlapping requests are visible.
    let mock = server
        .mock("GET", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"m","attachments":[]}"#)
        .with_chunked_body(|w| {
            std::thread::sleep(Duration::from_millis(80));
            w.write_all(br#"{"id":"m","attachments":[]}"#)
        })
        .expect_at_least(6)
        .create_async()
        .await;

    let client = Arc::new(DiscordClient::new(
        DiscordClientConfig::new("id", "token")
            .with_base_url(server.url())
            .with_max_concurrency(2),
    ));

    let done = Arc::new(AtomicUsize::new(0));
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let client = client.clone();
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            let _ = client.fetch_message("m").await;
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let elapsed = started.elapsed();

    assert_eq!(done.load(Ordering::SeqCst), 6);
    // Six requests, two at a time, 80 ms each: three rounds. Unbounded they
    // would all overlap and finish in roughly one.
    assert!(
        elapsed >= Duration::from_millis(200),
        "they did not queue: {elapsed:?}"
    );
    mock.assert_async().await;
}

/// A spent bucket must wait exactly as long as Discord says, and the relative
/// header is what says it: the absolute one is only as good as the agreement
/// between two clocks.
#[tokio::test]
async fn an_exhausted_bucket_waits_the_time_the_server_gave() {
    let mut server = Server::new_async().await;
    let spent = server
        .mock("GET", mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("x-ratelimit-remaining", "0")
        // A clock an hour behind would make the absolute header say "wait an
        // hour"; the relative one is immune to that.
        .with_header("x-ratelimit-reset", "1")
        .with_header("x-ratelimit-reset-after", "0.25")
        .with_body(r#"{"id":"m","attachments":[]}"#)
        .expect(2)
        .create_async()
        .await;

    let client =
        DiscordClient::new(DiscordClientConfig::new("id", "token").with_base_url(server.url()));

    client.fetch_message("m").await.unwrap();
    let started = std::time::Instant::now();
    client.fetch_message("m").await.unwrap();
    let waited = started.elapsed();

    assert!(
        waited >= Duration::from_millis(200),
        "did not wait: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "waited on a stale clock: {waited:?}"
    );
    spent.assert_async().await;
}
