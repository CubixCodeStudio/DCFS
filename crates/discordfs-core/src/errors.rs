//! Custom error types with context for better error handling.

use std::fmt;
use thiserror::Error;
use uuid::Uuid;

/// Base error type for DiscordFS operations.
#[derive(Debug, Error)]
pub enum DiscordFsError {
    #[error("Node error: {0}")]
    Node(#[from] NodeError),

    #[error("Version error: {0}")]
    Version(#[from] VersionError),

    #[error("Object error: {0}")]
    Object(#[from] ObjectError),

    #[error("Cache error: {0}")]
    Cache(#[from] CacheError),

    #[error("Crypto error: {0}")]
    Crypto(#[from] CryptoError),

    #[error("Discord error: {0}")]
    Discord(#[from] DiscordError),

    #[error("Database error: {0}")]
    Database(#[from] DatabaseError),

    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("Internal error: {0}")]
    Internal(#[from] InternalError),
}

/// Errors related to node operations.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("node not found: {id}")]
    NotFound { id: Uuid },

    #[error("node already exists: {name}")]
    AlreadyExists { name: String },

    #[error("invalid node name: {reason}")]
    InvalidName { reason: String },

    #[error("parent not found: {parent_id}")]
    ParentNotFound { parent_id: Uuid },

    #[error("cannot delete non-empty directory")]
    DirectoryNotEmpty,

    #[error("node is a directory, expected file")]
    IsDirectory,

    #[error("node is a file, expected directory")]
    IsFile,

    #[error("permission denied: {operation}")]
    PermissionDenied { operation: String },

    #[error("name too long: {len} bytes (max 255)")]
    NameTooLong { len: usize },

    #[error("path too long: {len} bytes (max 4096)")]
    PathTooLong { len: usize },
}

/// Errors related to version operations.
#[derive(Debug, Error)]
pub enum VersionError {
    #[error("version not found: {id}")]
    NotFound { id: Uuid },

    #[error("version already committed: {id}")]
    AlreadyCommitted { id: Uuid },

    #[error("version conflict: expected generation {expected}, got {actual}")]
    Conflict { expected: i64, actual: i64 },

    #[error("invalid version state: {state}")]
    InvalidState { state: String },

    #[error("chunk not found: index {index} in version {version_id}")]
    ChunkNotFound { index: u64, version_id: Uuid },

    #[error("chunk size mismatch: expected {expected}, got {actual}")]
    ChunkSizeMismatch { expected: u64, actual: u64 },

    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("version too large: {size} bytes (max {max})")]
    TooLarge { size: u64, max: u64 },
}

/// Errors related to object storage.
#[derive(Debug, Error)]
pub enum ObjectError {
    #[error("object not found: {id}")]
    NotFound { id: Uuid },

    #[error("object already exists: {id}")]
    AlreadyExists { id: Uuid },

    #[error("object corrupted: {id}")]
    Corrupted { id: Uuid },

    #[error("object too large: {size} bytes (max {max})")]
    TooLarge { size: u64, max: u64 },

    #[error("storage backend unavailable: {reason}")]
    BackendUnavailable { reason: String },

    #[error("storage quota exceeded: {used}/{limit} bytes")]
    QuotaExceeded { used: u64, limit: u64 },
}

/// Errors related to caching.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache miss: {key}")]
    Miss { key: String },

    #[error("cache full: {used}/{limit} bytes")]
    Full { used: u64, limit: u64 },

    #[error("cache corrupted: {reason}")]
    Corrupted { reason: String },

    #[error("cache write failed: {reason}")]
    WriteFailed { reason: String },

    #[error("cache read failed: {reason}")]
    ReadFailed { reason: String },

    #[error("journal recovery failed: {reason}")]
    RecoveryFailed { reason: String },
}

/// Errors related to encryption/decryption.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed: {reason}")]
    EncryptionFailed { reason: String },

    #[error("decryption failed: {reason}")]
    DecryptionFailed { reason: String },

    #[error("invalid key: {reason}")]
    InvalidKey { reason: String },

    #[error("authentication failed: data tampered")]
    AuthenticationFailed,

    #[error("nonce reuse detected")]
    NonceReuse,
}

/// Errors related to Discord API.
#[derive(Debug, Error)]
pub enum DiscordError {
    #[error("rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: f64 },

    #[error("API error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("attachment not found: {id}")]
    AttachmentNotFound { id: String },

    #[error("upload failed: {reason}")]
    UploadFailed { reason: String },

    #[error("download failed: {reason}")]
    DownloadFailed { reason: String },

    #[error("network error: {reason}")]
    NetworkError { reason: String },

    #[error("max retries exceeded: {attempts}")]
    MaxRetriesExceeded { attempts: u32 },

    #[error("invalid webhook: {reason}")]
    InvalidWebhook { reason: String },
}

/// Errors related to database operations.
#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("connection failed: {reason}")]
    ConnectionFailed { reason: String },

    #[error("query failed: {reason}")]
    QueryFailed { reason: String },

    #[error("transaction failed: {reason}")]
    TransactionFailed { reason: String },

    #[error("migration failed: {reason}")]
    MigrationFailed { reason: String },

    #[error("constraint violation: {constraint}")]
    ConstraintViolation { constraint: String },

    #[error("deadlock detected")]
    Deadlock,

    #[error("timeout: {operation}")]
    Timeout { operation: String },
}

/// Errors related to configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required field: {field}")]
    MissingField { field: String },

    #[error("invalid value for {field}: {reason}")]
    InvalidValue { field: String, reason: String },

    #[error("file not found: {path}")]
    FileNotFound { path: String },

    #[error("parse error: {reason}")]
    ParseError { reason: String },
}

/// Internal errors that shouldn't happen.
#[derive(Debug, Error)]
pub enum InternalError {
    #[error("invariant violated: {reason}")]
    InvariantViolated { reason: String },

    #[error("unreachable code reached: {location}")]
    Unreachable { location: String },

    #[error("resource exhausted: {resource}")]
    ResourceExhausted { resource: String },

    #[error("bug: {reason}")]
    Bug { reason: String },
}

/// Error with context for better debugging.
#[derive(Debug)]
pub struct ContextualError<E> {
    pub error: E,
    pub context: ErrorContext,
}

/// Context information for errors.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub operation: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub request_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub metadata: Vec<(String, String)>,
}

impl ErrorContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.operation = Some(operation.into());
        self
    }

    pub fn with_resource(
        mut self,
        resource_type: impl Into<String>,
        resource_id: impl Into<String>,
    ) -> Self {
        self.resource_type = Some(resource_type.into());
        self.resource_id = Some(resource_id.into());
        self
    }

    pub fn with_request_id(mut self, request_id: Uuid) -> Self {
        self.request_id = Some(request_id);
        self
    }

    pub fn with_user_id(mut self, user_id: Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.push((key.into(), value.into()));
        self
    }
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();

        if let Some(ref op) = self.operation {
            parts.push(format!("operation={}", op));
        }
        if let Some(ref res_type) = self.resource_type {
            parts.push(format!("resource_type={}", res_type));
        }
        if let Some(ref res_id) = self.resource_id {
            parts.push(format!("resource_id={}", res_id));
        }
        if let Some(ref req_id) = self.request_id {
            parts.push(format!("request_id={}", req_id));
        }
        if let Some(ref user_id) = self.user_id {
            parts.push(format!("user_id={}", user_id));
        }
        for (key, value) in &self.metadata {
            parts.push(format!("{}={}", key, value));
        }

        write!(f, "{}", parts.join(", "))
    }
}

impl<E: fmt::Display> fmt::Display for ContextualError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.error, self.context)
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ContextualError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Extension trait for adding context to errors.
///
/// The error variant is large by design: it carries the operation, resource and
/// request ids that make a failure diagnosable. Errors here are rare, so the
/// size costs nothing on the success path.
#[allow(clippy::result_large_err)]
pub trait ContextExt<T, E>: Sized {
    fn with_context<F>(self, f: F) -> Result<T, ContextualError<E>>
    where
        F: FnOnce() -> ErrorContext;
}

#[allow(clippy::result_large_err)]
impl<T, E> ContextExt<T, E> for Result<T, E> {
    fn with_context<F>(self, f: F) -> Result<T, ContextualError<E>>
    where
        F: FnOnce() -> ErrorContext,
    {
        self.map_err(|error| ContextualError {
            error,
            context: f(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_context_display() {
        let ctx = ErrorContext::new()
            .with_operation("create_node")
            .with_resource("node", "abc-123")
            .with_metadata("parent_id", "xyz-456");

        let display = format!("{}", ctx);
        assert!(display.contains("operation=create_node"));
        assert!(display.contains("resource_type=node"));
        assert!(display.contains("resource_id=abc-123"));
        assert!(display.contains("parent_id=xyz-456"));
    }

    #[test]
    fn test_contextual_error() {
        let error = NodeError::NotFound { id: Uuid::new_v4() };
        let ctx = ErrorContext::new().with_operation("get_node");
        let contextual = ContextualError {
            error,
            context: ctx,
        };

        let display = format!("{}", contextual);
        assert!(display.contains("node not found"));
        assert!(display.contains("operation=get_node"));
    }

    #[test]
    fn test_context_extension() {
        let result: Result<(), NodeError> = Err(NodeError::NotFound { id: Uuid::new_v4() });

        let contextual = result.with_context(|| {
            ErrorContext::new()
                .with_operation("test_operation")
                .with_resource("test_resource", "test_id")
        });

        assert!(contextual.is_err());
        let err = contextual.unwrap_err();
        assert!(format!("{}", err.context).contains("operation=test_operation"));
    }
}
