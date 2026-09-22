//! Discord API client with rate limiting, retry, and ObjectStore adapter.
//!
//! This crate provides a Discord REST client for uploading/downloading
//! attachments via webhooks, with rate limit bucket tracking, 429 handling,
//! and exponential backoff retry. It also implements the `ObjectStore` trait
//! so Discord can be used as an immutable object storage backend.

pub mod client;
pub mod locator;
pub mod ratelimit;
pub mod retry;
pub mod store;

pub use client::{DiscordClient, DiscordClientConfig, DiscordError};
pub use locator::DiscordLocator;
pub use ratelimit::RateLimitBucket;
pub use retry::{RetryConfig, RetryPolicy};
pub use store::{DiscordObjectStore, LocatorStore, MemoryLocatorStore};
