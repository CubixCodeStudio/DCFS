//! Rate limit bucket tracking for Discord API.
//!
//! Discord returns rate limit headers on every response:
//! - `X-RateLimit-Limit`: max requests in the window
//! - `X-RateLimit-Remaining`: remaining requests
//! - `X-RateLimit-Reset`: Unix timestamp when the bucket resets
//! - `X-RateLimit-Bucket`: the bucket hash
//!
//! On 429, a `Retry-After` header (seconds) tells us how long to wait.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tracks rate limit state for a single bucket.
#[derive(Debug)]
pub struct RateLimitBucket {
    inner: Arc<Mutex<BucketState>>,
}

impl Clone for RateLimitBucket {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug, Clone)]
struct BucketState {
    remaining: Option<u64>,
    reset_at: Option<Instant>,
    /// Global rate limit (applies across all buckets).
    global: bool,
}

impl RateLimitBucket {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BucketState {
                remaining: None,
                reset_at: None,
                global: false,
            })),
        }
    }

    /// Update bucket state from response headers.
    pub fn update_from_headers(
        &self,
        remaining: Option<u64>,
        reset_at: Option<Instant>,
        global: bool,
    ) {
        let mut state = self.inner.lock();
        state.remaining = remaining;
        state.reset_at = reset_at;
        state.global = global;
    }

    /// Mark a 429 response — caller must wait for `retry_after`.
    pub fn mark_rate_limited(&self, retry_after: Duration) {
        let mut state = self.inner.lock();
        state.remaining = Some(0);
        state.reset_at = Some(Instant::now() + retry_after);
    }

    /// How long to wait before the next request, if the bucket is spent.
    ///
    /// Only a bucket with nothing left to spend makes a caller wait. Waiting
    /// whenever the window is merely open — which is to say, after every
    /// successful response — spends one request per window instead of the
    /// whole allowance, and turns a multi-part upload into one part per reset.
    pub fn wait_duration(&self) -> Option<Duration> {
        let state = self.inner.lock();
        if state.remaining.unwrap_or(1) > 0 {
            return None;
        }
        let reset_at = state.reset_at?;
        let now = Instant::now();
        if now >= reset_at {
            None
        } else {
            Some(reset_at - now)
        }
    }

    /// Whether the bucket is currently rate limited.
    pub fn is_limited(&self) -> bool {
        self.wait_duration().is_some()
    }
}

impl Default for RateLimitBucket {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages multiple rate limit buckets keyed by bucket hash.
#[derive(Debug)]
pub struct RateLimitManager {
    buckets: Mutex<HashMap<String, RateLimitBucket>>,
    global: RateLimitBucket,
}

impl RateLimitManager {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            global: RateLimitBucket::new(),
        }
    }

    /// Get or create a bucket for the given hash.
    pub fn bucket(&self, hash: &str) -> RateLimitBucket {
        let mut buckets = self.buckets.lock();
        buckets.entry(hash.to_string()).or_default().clone()
    }

    /// The global rate limit bucket.
    pub fn global(&self) -> &RateLimitBucket {
        &self.global
    }

    /// Wait for both the global and bucket-specific limits.
    pub async fn wait_for_slot(&self, bucket_hash: Option<&str>) {
        // Wait for global first.
        if let Some(wait) = self.global.wait_duration() {
            tokio::time::sleep(wait).await;
        }
        // Then bucket-specific.
        let wait_duration = if let Some(hash) = bucket_hash {
            let buckets = self.buckets.lock();
            buckets.get(hash).and_then(|b| b.wait_duration())
        } else {
            None
        };
        if let Some(wait) = wait_duration {
            tokio::time::sleep(wait).await;
        }
    }
}

impl Default for RateLimitManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_starts_unlimited() {
        let b = RateLimitBucket::new();
        assert!(!b.is_limited());
        assert!(b.wait_duration().is_none());
    }

    #[test]
    fn bucket_tracks_remaining() {
        let b = RateLimitBucket::new();
        // Set reset_at in the past so it's not rate limited.
        b.update_from_headers(
            Some(5),
            Some(Instant::now() - Duration::from_secs(1)),
            false,
        );
        assert!(!b.is_limited());
    }

    /// The window being open is not a reason to wait: only an exhausted
    /// allowance is. Waiting on headroom spent one request per window.
    #[test]
    fn headroom_is_not_a_rate_limit() {
        let b = RateLimitBucket::new();
        b.update_from_headers(
            Some(4),
            Some(Instant::now() + Duration::from_secs(5)),
            false,
        );
        assert!(!b.is_limited(), "4 requests left is not rate limited");
        assert_eq!(b.wait_duration(), None);
    }

    #[test]
    fn an_exhausted_bucket_waits_for_its_reset() {
        let b = RateLimitBucket::new();
        b.update_from_headers(
            Some(0),
            Some(Instant::now() + Duration::from_secs(2)),
            false,
        );
        assert!(b.is_limited());
        assert!(b.wait_duration().unwrap() <= Duration::from_secs(2));
    }

    /// A bucket nothing is known about yet must not block the first request.
    #[test]
    fn an_unknown_bucket_does_not_wait() {
        let b = RateLimitBucket::new();
        b.update_from_headers(None, Some(Instant::now() + Duration::from_secs(5)), false);
        assert!(!b.is_limited());
    }

    #[test]
    fn bucket_rate_limited() {
        let b = RateLimitBucket::new();
        b.mark_rate_limited(Duration::from_secs(2));
        assert!(b.is_limited());
        let wait = b.wait_duration().unwrap();
        assert!(wait <= Duration::from_secs(2));
    }

    #[test]
    fn manager_creates_buckets() {
        let mgr = RateLimitManager::new();
        let b1 = mgr.bucket("abc");
        let b2 = mgr.bucket("abc");
        // Same bucket hash returns the same bucket.
        b1.mark_rate_limited(Duration::from_secs(1));
        assert!(b2.is_limited());
    }
}
