//! Error handling for the server.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use discordfs_db::RepositoryError;
use discordfs_objectstore::ObjectStoreError;
use discordfs_protocol::{ApiError, ErrorCode};
use std::fmt;
use tracing::error;
use uuid::Uuid;

/// Application error type that maps to HTTP responses.
pub struct AppError {
    status: StatusCode,
    error: ApiError,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    context: ErrorContext,
}

/// Additional context for error reporting.
#[derive(Debug, Default, Clone)]
pub struct ErrorContext {
    pub request_id: Option<Uuid>,
    pub operation: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
}

impl ErrorContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_request_id(mut self, request_id: Uuid) -> Self {
        self.request_id = Some(request_id);
        self
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
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(ref req_id) = self.request_id {
            parts.push(format!("request_id={}", req_id));
        }
        if let Some(ref op) = self.operation {
            parts.push(format!("operation={}", op));
        }
        if let Some(ref res_type) = self.resource_type {
            parts.push(format!("resource_type={}", res_type));
        }
        if let Some(ref res_id) = self.resource_id {
            parts.push(format!("resource_id={}", res_id));
        }
        write!(f, "{}", parts.join(", "))
    }
}

impl AppError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: ApiError::new(ErrorCode::NotFound, message),
            source: None,
            context: ErrorContext::default(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: ApiError::new(ErrorCode::InvalidRequest, message),
            source: None,
            context: ErrorContext::default(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: ApiError::new(ErrorCode::Conflict, message),
            source: None,
            context: ErrorContext::default(),
        }
    }

    /// A directory still has children. POSIX wants ENOTEMPTY here, which the
    /// client can only produce from a code of its own.
    pub fn directory_not_empty() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: ApiError::new(ErrorCode::DirectoryNotEmpty, "directory is not empty"),
            source: None,
            context: ErrorContext::default(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: ApiError::new(ErrorCode::BackendUnavailable, message),
            source: None,
            context: ErrorContext::default(),
        }
    }

    /// Add error context for better debugging.
    pub fn with_context(mut self, context: ErrorContext) -> Self {
        self.context = context;
        self
    }

    /// Add source error for error chain.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log error with context for debugging
        if self.status.is_server_error() {
            error!(
                status = %self.status,
                error_code = ?self.error.code,
                message = %self.error.message,
                context = %self.context,
                source = ?self.source,
                "Server error"
            );
        }

        // Include context in error response
        let mut error = self.error;
        if let Some(request_id) = self.context.request_id {
            error.request_id = Some(request_id.to_string());
        }

        (self.status, Json(error)).into_response()
    }
}

impl From<RepositoryError> for AppError {
    fn from(err: RepositoryError) -> Self {
        match err {
            RepositoryError::NotFound => Self::not_found("resource not found"),
            RepositoryError::AlreadyExists => Self::conflict("resource already exists"),
            RepositoryError::Conflict => Self::conflict("optimistic concurrency conflict"),
            RepositoryError::DirectoryNotEmpty => Self::directory_not_empty(),
            // Backend detail can carry the DSN or schema internals: log it, return a
            // generic message to the caller.
            RepositoryError::Database(msg) => {
                error!(detail = %msg, "repository backend error");
                Self::internal("backend unavailable")
            }
        }
    }
}

impl From<ObjectStoreError> for AppError {
    fn from(err: ObjectStoreError) -> Self {
        match err {
            ObjectStoreError::NotFound(_) => Self::not_found("object not found"),
            ObjectStoreError::AlreadyExists(_) => Self::conflict("object already exists"),
            ObjectStoreError::Backend(msg) => {
                error!(detail = %msg, "object store backend error");
                Self::internal("backend unavailable")
            }
        }
    }
}

impl From<uuid::Error> for AppError {
    fn from(_: uuid::Error) -> Self {
        Self::bad_request("invalid UUID format")
    }
}

impl From<discordfs_core::NodeNameError> for AppError {
    fn from(err: discordfs_core::NodeNameError) -> Self {
        Self::bad_request(format!("invalid name: {}", err))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        Self::bad_request(format!("invalid JSON: {}", err))
    }
}
