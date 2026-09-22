//! Advanced retry logic with exponential backoff and jitter.

use std::future::Future;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, warn};

/// Retry configuration with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_attempts: u32,
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
    /// Backoff multiplier.
    pub multiplier: f64,
    /// Whether to add jitter to backoff.
    pub jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
            multiplier: 2.0,
            jitter: true,
        }
    }
}

impl RetryConfig {
    /// Create a new retry configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum attempts.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Set initial backoff.
    pub fn with_initial_backoff(mut self, backoff: Duration) -> Self {
        self.initial_backoff = backoff;
        self
    }

    /// Set maximum backoff.
    pub fn with_max_backoff(mut self, backoff: Duration) -> Self {
        self.max_backoff = backoff;
        self
    }

    /// Set multiplier.
    pub fn with_multiplier(mut self, multiplier: f64) -> Self {
        self.multiplier = multiplier;
        self
    }

    /// Enable or disable jitter.
    pub fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }

    /// Calculate backoff duration for a given attempt.
    pub fn backoff_duration(&self, attempt: u32) -> Duration {
        let base = self.initial_backoff.as_millis() as f64 * self.multiplier.powi(attempt as i32);
        let mut duration = Duration::from_millis(base as u64);

        // Cap at max_backoff
        if duration > self.max_backoff {
            duration = self.max_backoff;
        }

        // Add jitter if enabled
        if self.jitter {
            let jitter_range = duration.as_millis() as f64 * 0.2; // ±20%
            let jitter = (rand::random::<f64>() - 0.5) * 2.0 * jitter_range;
            let jittered = duration.as_millis() as f64 + jitter;
            duration = Duration::from_millis(jittered.max(0.0) as u64);
        }

        duration
    }
}

/// Result of a retry operation.
#[derive(Debug)]
pub struct RetryResult<T> {
    /// The final result.
    pub result: Result<T, RetryError>,
    /// Number of attempts made.
    pub attempts: u32,
    /// Total time spent.
    pub elapsed: Duration,
}

/// Error during retry operations.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("max retries ({max_attempts}) exceeded: {last_error}")]
    MaxRetriesExceeded {
        max_attempts: u32,
        last_error: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("non-retryable error: {0}")]
    NonRetryable(Box<dyn std::error::Error + Send + Sync>),

    #[error("operation cancelled")]
    Cancelled,
}

/// Trait for determining if an error is retryable.
pub trait Retryable {
    /// Returns true if the operation should be retried.
    fn is_retryable(&self) -> bool;
}

/// Retry an async operation with exponential backoff.
pub async fn retry_with_backoff<F, Fut, T, E>(
    config: &RetryConfig,
    mut operation: F,
) -> RetryResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::error::Error + Retryable + Send + Sync + 'static,
{
    let start = Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;

        match operation().await {
            Ok(value) => {
                return RetryResult {
                    result: Ok(value),
                    attempts,
                    elapsed: start.elapsed(),
                };
            }
            Err(err) => {
                if !err.is_retryable() {
                    debug!(
                        attempt = attempts,
                        error = %err,
                        "Non-retryable error, stopping"
                    );
                    return RetryResult {
                        result: Err(RetryError::NonRetryable(Box::new(err))),
                        attempts,
                        elapsed: start.elapsed(),
                    };
                }

                if attempts >= config.max_attempts {
                    warn!(
                        attempts,
                        max_attempts = config.max_attempts,
                        error = %err,
                        "Max retries exceeded"
                    );
                    return RetryResult {
                        result: Err(RetryError::MaxRetriesExceeded {
                            max_attempts: config.max_attempts,
                            last_error: Box::new(err),
                        }),
                        attempts,
                        elapsed: start.elapsed(),
                    };
                }

                let backoff = config.backoff_duration(attempts);
                debug!(
                    attempt = attempts,
                    backoff_ms = backoff.as_millis(),
                    error = %err,
                    "Retryable error, backing off"
                );
                sleep(backoff).await;
            }
        }
    }
}

/// Retry with a predicate function instead of trait.
pub async fn retry_with_predicate<F, Fut, T, E>(
    config: &RetryConfig,
    mut operation: F,
    is_retryable: impl Fn(&E) -> bool,
) -> RetryResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let start = Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;

        match operation().await {
            Ok(value) => {
                return RetryResult {
                    result: Ok(value),
                    attempts,
                    elapsed: start.elapsed(),
                };
            }
            Err(err) => {
                if !is_retryable(&err) {
                    debug!(
                        attempt = attempts,
                        error = %err,
                        "Non-retryable error, stopping"
                    );
                    return RetryResult {
                        result: Err(RetryError::NonRetryable(Box::new(err))),
                        attempts,
                        elapsed: start.elapsed(),
                    };
                }

                if attempts >= config.max_attempts {
                    warn!(
                        attempts,
                        max_attempts = config.max_attempts,
                        error = %err,
                        "Max retries exceeded"
                    );
                    return RetryResult {
                        result: Err(RetryError::MaxRetriesExceeded {
                            max_attempts: config.max_attempts,
                            last_error: Box::new(err),
                        }),
                        attempts,
                        elapsed: start.elapsed(),
                    };
                }

                let backoff = config.backoff_duration(attempts);
                debug!(
                    attempt = attempts,
                    backoff_ms = backoff.as_millis(),
                    error = %err,
                    "Retryable error, backing off"
                );
                sleep(backoff).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[derive(Debug, thiserror::Error)]
    #[error("test error: {message}")]
    struct TestError {
        message: String,
        retryable: bool,
    }

    impl Retryable for TestError {
        fn is_retryable(&self) -> bool {
            self.retryable
        }
    }

    #[tokio::test]
    async fn test_retry_success_first_attempt() {
        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, || async { Ok::<_, TestError>(42) }).await;

        assert!(result.result.is_ok());
        assert_eq!(result.result.unwrap(), 42);
        assert_eq!(result.attempts, 1);
    }

    #[tokio::test]
    async fn test_retry_success_after_failures() {
        let config = RetryConfig::new()
            .with_max_attempts(3)
            .with_initial_backoff(Duration::from_millis(10));

        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result = retry_with_backoff(&config, move || {
            let attempts = attempts_clone.clone();
            async move {
                let current = attempts.fetch_add(1, Ordering::SeqCst);
                if current < 2 {
                    Err(TestError {
                        message: "temporary failure".to_string(),
                        retryable: true,
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert!(result.result.is_ok());
        assert_eq!(result.attempts, 3);
    }

    #[tokio::test]
    async fn test_retry_max_attempts_exceeded() {
        let config = RetryConfig::new()
            .with_max_attempts(2)
            .with_initial_backoff(Duration::from_millis(10));

        let result = retry_with_backoff(&config, || async {
            Err::<(), _>(TestError {
                message: "persistent failure".to_string(),
                retryable: true,
            })
        })
        .await;

        assert!(matches!(
            result.result,
            Err(RetryError::MaxRetriesExceeded { .. })
        ));
        assert_eq!(result.attempts, 2);
    }

    #[tokio::test]
    async fn test_retry_non_retryable_error() {
        let config = RetryConfig::new()
            .with_max_attempts(5)
            .with_initial_backoff(Duration::from_millis(10));

        let result = retry_with_backoff(&config, || async {
            Err::<(), _>(TestError {
                message: "permanent failure".to_string(),
                retryable: false,
            })
        })
        .await;

        assert!(matches!(result.result, Err(RetryError::NonRetryable(_))));
        assert_eq!(result.attempts, 1);
    }

    #[test]
    fn test_backoff_calculation() {
        let config = RetryConfig::new()
            .with_initial_backoff(Duration::from_millis(100))
            .with_max_backoff(Duration::from_secs(10))
            .with_multiplier(2.0)
            .with_jitter(false);

        assert_eq!(config.backoff_duration(0), Duration::from_millis(100));
        assert_eq!(config.backoff_duration(1), Duration::from_millis(200));
        assert_eq!(config.backoff_duration(2), Duration::from_millis(400));
        assert_eq!(config.backoff_duration(3), Duration::from_millis(800));
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let config = RetryConfig::new()
            .with_initial_backoff(Duration::from_millis(100))
            .with_max_backoff(Duration::from_millis(500))
            .with_multiplier(2.0)
            .with_jitter(false);

        assert_eq!(config.backoff_duration(10), Duration::from_millis(500));
    }
}
