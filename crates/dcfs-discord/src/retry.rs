//! Retry policy with exponential backoff.

use std::time::Duration;

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_attempts: u32,
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
    /// Multiplier for exponential backoff.
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

/// Determines whether a request should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// Retry the request.
    Retry,
    /// Do not retry, fail immediately.
    Fail,
}

impl RetryConfig {
    /// Calculate the backoff duration for the given attempt (0-indexed).
    pub fn backoff_duration(&self, attempt: u32) -> Duration {
        let base = self.initial_backoff.as_secs_f64();
        let multiplier = self.multiplier.powi(attempt as i32);
        let backoff_secs = (base * multiplier).min(self.max_backoff.as_secs_f64());
        Duration::from_secs_f64(backoff_secs)
    }

    /// Whether we should retry given the current attempt number.
    pub fn should_retry(&self, attempt: u32) -> RetryPolicy {
        if attempt < self.max_attempts {
            RetryPolicy::Retry
        } else {
            RetryPolicy::Fail
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases_exponentially() {
        let config = RetryConfig::default();
        let d0 = config.backoff_duration(0);
        let d1 = config.backoff_duration(1);
        let d2 = config.backoff_duration(2);
        assert!(d1 > d0);
        assert!(d2 > d1);
    }

    #[test]
    fn backoff_capped_at_max() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5),
            multiplier: 10.0,
        };
        let d = config.backoff_duration(5);
        assert!(d <= Duration::from_secs(5));
    }

    #[test]
    fn should_retry_respects_max_attempts() {
        let config = RetryConfig {
            max_attempts: 3,
            ..Default::default()
        };
        assert_eq!(config.should_retry(0), RetryPolicy::Retry);
        assert_eq!(config.should_retry(2), RetryPolicy::Retry);
        assert_eq!(config.should_retry(3), RetryPolicy::Fail);
    }
}
