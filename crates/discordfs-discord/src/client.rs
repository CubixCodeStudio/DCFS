//! Discord REST client for webhook attachments.

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, warn};

use crate::ratelimit::RateLimitManager;
use crate::retry::{RetryConfig, RetryPolicy};

/// Errors from the Discord client.
#[derive(Debug, Error)]
pub enum DiscordError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Discord API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: f64 },
    #[error("max retries ({max_attempts}) exceeded")]
    MaxRetriesExceeded { max_attempts: u32 },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("attachment not found in response")]
    AttachmentNotFound,
}

/// Configuration for the Discord client.
///
/// `Debug` is written by hand: a webhook token is a credential, and deriving
/// it would put one into any log line that formats the config.
#[derive(Clone)]
pub struct DiscordClientConfig {
    /// Webhook ID.
    pub webhook_id: String,
    /// Webhook token.
    pub webhook_token: String,
    /// Base URL (override for testing).
    pub base_url: String,
    /// Retry configuration.
    pub retry: RetryConfig,
    /// How many requests may be in flight at once.
    ///
    /// Rate-limit headers describe what has already happened, so a burst of
    /// parallel requests all read the same "plenty left" state and arrive
    /// together. A bound keeps a wide filesystem write from turning into a
    /// 429 storm that every request then has to back off from.
    pub max_concurrency: usize,
}

impl std::fmt::Debug for DiscordClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordClientConfig")
            .field("webhook_id", &self.webhook_id)
            .field("webhook_token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("retry", &self.retry)
            .field("max_concurrency", &self.max_concurrency)
            .finish()
    }
}

impl DiscordClientConfig {
    pub fn new(webhook_id: impl Into<String>, webhook_token: impl Into<String>) -> Self {
        Self {
            webhook_id: webhook_id.into(),
            webhook_token: webhook_token.into(),
            base_url: "https://discord.com/api/v10".to_string(),
            retry: RetryConfig::default(),
            max_concurrency: 4,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency.max(1);
        self
    }
}

/// Discord webhook message response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookMessage {
    pub id: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// A Discord attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub size: u64,
    pub url: String,
    pub proxy_url: String,
}

/// Discord REST client.
#[derive(Clone)]
pub struct DiscordClient {
    config: Arc<DiscordClientConfig>,
    http: reqwest::Client,
    ratelimits: Arc<RateLimitManager>,
    /// Bounds requests in flight. See `DiscordClientConfig::max_concurrency`.
    inflight: Arc<tokio::sync::Semaphore>,
}

impl DiscordClient {
    pub fn new(config: DiscordClientConfig) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        let inflight = Arc::new(tokio::sync::Semaphore::new(config.max_concurrency.max(1)));
        Self {
            config: Arc::new(config),
            http,
            ratelimits: Arc::new(RateLimitManager::new()),
            inflight,
        }
    }

    /// Build with a custom reqwest client (for testing).
    /// Which webhook this client posts as.
    pub fn webhook_id(&self) -> &str {
        &self.config.webhook_id
    }

    pub fn with_http_client(config: DiscordClientConfig, http: reqwest::Client) -> Self {
        let inflight = Arc::new(tokio::sync::Semaphore::new(config.max_concurrency.max(1)));
        Self {
            config: Arc::new(config),
            http,
            ratelimits: Arc::new(RateLimitManager::new()),
            inflight,
        }
    }

    /// Remove the webhook token from anything about to be logged or returned.
    ///
    /// Discord puts the credential in the URL path, and a `reqwest::Error`
    /// prints the URL it failed on, so an ordinary connection error would
    /// otherwise write a working credential into the log.
    fn scrub(&self, text: String) -> String {
        if self.config.webhook_token.is_empty() {
            return text;
        }
        text.replace(&self.config.webhook_token, "<webhook-token>")
    }

    /// Turn a failed request into an error that carries no credential.
    fn transport_error(&self, error: reqwest::Error) -> DiscordError {
        DiscordError::Http(self.scrub(error.to_string()))
    }

    /// Decide what to do about a request that never got a response.
    ///
    /// A dropped connection, a DNS hiccup or a timeout is exactly the case a
    /// retry exists for, and it used to be the one case that was not retried:
    /// the error was returned straight to the caller, so a moment of lost
    /// connectivity failed the operation outright.
    async fn handle_transport_error(
        &self,
        error: reqwest::Error,
        attempt: u32,
    ) -> Result<(), DiscordError> {
        if self.config.retry.should_retry(attempt) == RetryPolicy::Fail {
            return Err(self.transport_error(error));
        }
        let backoff = self.config.retry.backoff_duration(attempt);
        tracing::debug!(
            attempt,
            backoff_ms = backoff.as_millis(),
            error = %self.scrub(error.to_string()),
            "discord request failed to reach the server; retrying"
        );
        tokio::time::sleep(backoff).await;
        Ok(())
    }

    /// A single message belonging to this webhook. Executing the webhook posts
    /// to the webhook itself; reading or deleting what it posted addresses the
    /// message under `/messages`.
    fn message_url(&self, message_id: &str) -> String {
        format!("{}/messages/{}", self.webhook_url(), message_id)
    }

    fn webhook_url(&self) -> String {
        format!(
            "{}/webhooks/{}/{}",
            self.config.base_url, self.config.webhook_id, self.config.webhook_token
        )
    }

    /// Re-read a message this webhook posted.
    ///
    /// Attachment URLs expire, so a locator keeps the message and attachment
    /// ids — which do not — and asks for a fresh URL when the cached one stops
    /// working.
    pub async fn fetch_message(&self, message_id: &str) -> Result<WebhookMessage, DiscordError> {
        let url = self.message_url(message_id);
        let bucket_hash = Some("webhook_fetch");

        for attempt in 0..=self.config.retry.max_attempts {
            self.ratelimits.wait_for_slot(bucket_hash).await;
            let permit = self
                .inflight
                .acquire()
                .await
                .expect("the semaphore is never closed");

            let resp = match self.http.get(&url).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    drop(permit);
                    self.handle_transport_error(e, attempt).await?;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            self.update_ratelimit(resp.headers(), bucket_hash);

            if status == 429 {
                let retry_after = parse_retry_after(resp.headers()).unwrap_or(1.0);
                self.ratelimits
                    .bucket("webhook_fetch")
                    .mark_rate_limited(Duration::from_secs_f64(retry_after));
                if self.config.retry.should_retry(attempt) == RetryPolicy::Fail {
                    return Err(DiscordError::MaxRetriesExceeded {
                        max_attempts: self.config.retry.max_attempts,
                    });
                }
                drop(permit);
                tokio::time::sleep(self.config.retry.backoff_duration(attempt)).await;
                continue;
            }

            if status == 404 {
                return Err(DiscordError::Api {
                    status: 404,
                    message: "message not found".to_string(),
                });
            }

            if !resp.status().is_success() {
                if status >= 500 && self.config.retry.should_retry(attempt) == RetryPolicy::Retry {
                    tokio::time::sleep(self.config.retry.backoff_duration(attempt)).await;
                    continue;
                }
                return Err(DiscordError::Api {
                    status,
                    message: resp.text().await.unwrap_or_default(),
                });
            }

            return resp.json().await.map_err(|e| self.transport_error(e));
        }

        Err(DiscordError::MaxRetriesExceeded {
            max_attempts: self.config.retry.max_attempts,
        })
    }

    /// Upload a file as an attachment via webhook.
    pub async fn upload_attachment(
        &self,
        filename: &str,
        data: Bytes,
    ) -> Result<WebhookMessage, DiscordError> {
        let url = self.webhook_url();
        let bucket_hash = Some("webhook_upload");

        for attempt in 0..=self.config.retry.max_attempts {
            // Wait for rate limit slot.
            self.ratelimits.wait_for_slot(bucket_hash).await;
            let permit = self
                .inflight
                .acquire()
                .await
                .expect("the semaphore is never closed");

            let part = reqwest::multipart::Part::bytes(data.to_vec())
                .file_name(filename.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e: reqwest::Error| {
                    DiscordError::InvalidResponse(self.scrub(e.to_string()))
                })?;
            let form = reqwest::multipart::Form::new().part("file", part);

            let resp = match self.http.post(&url).multipart(form).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    drop(permit);
                    self.handle_transport_error(e, attempt).await?;
                    continue;
                }
            };
            let status = resp.status().as_u16();

            // Update rate limit headers.
            self.update_ratelimit(resp.headers(), bucket_hash);

            if status == 429 {
                let retry_after = parse_retry_after(resp.headers()).unwrap_or(1.0);
                warn!(retry_after, "Discord rate limited (429)");
                self.ratelimits
                    .bucket("webhook_upload")
                    .mark_rate_limited(Duration::from_secs_f64(retry_after));
                if self.config.retry.should_retry(attempt) == RetryPolicy::Fail {
                    return Err(DiscordError::MaxRetriesExceeded {
                        max_attempts: self.config.retry.max_attempts,
                    });
                }
                drop(permit);
                tokio::time::sleep(self.config.retry.backoff_duration(attempt)).await;
                continue;
            }

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                if status >= 500 && self.config.retry.should_retry(attempt) == RetryPolicy::Retry {
                    debug!(status, "server error, retrying");
                    let backoff = self.config.retry.backoff_duration(attempt);
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(DiscordError::Api {
                    status,
                    message: body,
                });
            }

            let msg: WebhookMessage = resp.json().await.map_err(|e| self.transport_error(e))?;
            return Ok(msg);
        }

        Err(DiscordError::MaxRetriesExceeded {
            max_attempts: self.config.retry.max_attempts,
        })
    }

    /// Download an attachment by URL.
    pub async fn download_attachment(&self, url: &str) -> Result<Bytes, DiscordError> {
        self.download_attachment_range(url, None).await
    }

    /// Download an attachment, optionally only the bytes in `range`.
    ///
    /// A CDN that ignores `Range` answers 200 with the whole object, so the
    /// slice is applied here either way and a range is an optimisation, never
    /// something correctness rests on.
    pub async fn download_attachment_range(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
    ) -> Result<Bytes, DiscordError> {
        let bucket_hash = Some("webhook_download");

        for attempt in 0..=self.config.retry.max_attempts {
            self.ratelimits.wait_for_slot(bucket_hash).await;
            let permit = self
                .inflight
                .acquire()
                .await
                .expect("the semaphore is never closed");

            let mut request = self.http.get(url);
            if let Some((offset, len)) = range {
                request = request.header("range", format!("bytes={}-{}", offset, offset + len - 1));
            }
            let resp = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    drop(permit);
                    self.handle_transport_error(e, attempt).await?;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            self.update_ratelimit(resp.headers(), bucket_hash);

            if status == 429 {
                let retry_after = parse_retry_after(resp.headers()).unwrap_or(1.0);
                self.ratelimits
                    .bucket("webhook_download")
                    .mark_rate_limited(Duration::from_secs_f64(retry_after));
                if self.config.retry.should_retry(attempt) == RetryPolicy::Fail {
                    return Err(DiscordError::MaxRetriesExceeded {
                        max_attempts: self.config.retry.max_attempts,
                    });
                }
                drop(permit);
                tokio::time::sleep(self.config.retry.backoff_duration(attempt)).await;
                continue;
            }

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                if status >= 500 && self.config.retry.should_retry(attempt) == RetryPolicy::Retry {
                    let backoff = self.config.retry.backoff_duration(attempt);
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(DiscordError::Api {
                    status,
                    message: body,
                });
            }

            let partial = status == 206;
            let bytes = resp.bytes().await.map_err(|e| self.transport_error(e))?;
            let bytes = match range {
                // 200 means the range was ignored and this is the whole object.
                Some((offset, len)) if !partial => {
                    let start = (offset as usize).min(bytes.len());
                    let end = (start + len as usize).min(bytes.len());
                    bytes.slice(start..end)
                }
                _ => bytes,
            };
            return Ok(bytes);
        }

        Err(DiscordError::MaxRetriesExceeded {
            max_attempts: self.config.retry.max_attempts,
        })
    }

    /// Delete a webhook message (used for object deletion).
    pub async fn delete_message(&self, message_id: &str) -> Result<(), DiscordError> {
        let url = self.message_url(message_id);
        let bucket_hash = Some("webhook_delete");

        for attempt in 0..=self.config.retry.max_attempts {
            self.ratelimits.wait_for_slot(bucket_hash).await;
            let permit = self
                .inflight
                .acquire()
                .await
                .expect("the semaphore is never closed");

            let resp = match self.http.delete(&url).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    drop(permit);
                    self.handle_transport_error(e, attempt).await?;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            self.update_ratelimit(resp.headers(), bucket_hash);

            if status == 429 {
                let retry_after = parse_retry_after(resp.headers()).unwrap_or(1.0);
                self.ratelimits
                    .bucket("webhook_delete")
                    .mark_rate_limited(Duration::from_secs_f64(retry_after));
                if self.config.retry.should_retry(attempt) == RetryPolicy::Fail {
                    return Err(DiscordError::MaxRetriesExceeded {
                        max_attempts: self.config.retry.max_attempts,
                    });
                }
                drop(permit);
                tokio::time::sleep(self.config.retry.backoff_duration(attempt)).await;
                continue;
            }

            if status == 404 {
                return Err(DiscordError::Api {
                    status: 404,
                    message: "message not found".to_string(),
                });
            }

            if !resp.status().is_success() {
                if status >= 500 && self.config.retry.should_retry(attempt) == RetryPolicy::Retry {
                    let backoff = self.config.retry.backoff_duration(attempt);
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return Err(DiscordError::Api {
                    status,
                    message: body,
                });
            }

            return Ok(());
        }

        Err(DiscordError::MaxRetriesExceeded {
            max_attempts: self.config.retry.max_attempts,
        })
    }

    /// Update rate limit state from response headers.
    fn update_ratelimit(&self, headers: &HeaderMap, bucket_hash: Option<&str>) {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        // Prefer the relative header when the server sends one: the absolute
        // timestamp is only as good as the agreement between two clocks, and a
        // local clock that runs fast turns into a wait that never ends.
        let reset = headers
            .get("x-ratelimit-reset-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|after| after.is_finite() && *after >= 0.0)
            .map(|after| Instant::now() + Duration::from_secs_f64(after))
            .or_else(|| {
                headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|ts| {
                        let reset_unix = ts as u64;
                        let now_unix = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        Instant::now() + Duration::from_secs(reset_unix.saturating_sub(now_unix))
                    })
            });
        let global = headers
            .get("x-ratelimit-global")
            .map(|v| v == HeaderValue::from_static("true"))
            .unwrap_or(false);

        if global {
            self.ratelimits
                .global()
                .update_from_headers(remaining, reset, true);
        } else if let Some(hash) = bucket_hash {
            self.ratelimits
                .bucket(hash)
                .update_from_headers(remaining, reset, false);
        }
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<f64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = DiscordClientConfig::new("id", "token");
        assert_eq!(cfg.base_url, "https://discord.com/api/v10");
        assert_eq!(cfg.retry.max_attempts, 5);
    }

    #[test]
    fn webhook_url_format() {
        let cfg = DiscordClientConfig::new("123", "abc").with_base_url("http://localhost:9999");
        let client = DiscordClient::new(cfg);
        assert_eq!(
            client.webhook_url(),
            "http://localhost:9999/webhooks/123/abc"
        );
    }
}
