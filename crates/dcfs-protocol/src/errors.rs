//! Stable API error codes.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// API error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    InvalidName,
    Conflict,
    DirectoryNotEmpty,
    Integrity,
    QueueFull,
    Unauthorized,
    BackendUnavailable,
    InvalidRequest,
}

/// API error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub request_id: Option<String>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            request_id: None,
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

/// Domain error type for API operations.
#[derive(Debug, Error)]
pub enum ApiErrorKind {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("invalid name")]
    InvalidName,
    #[error("conflict")]
    Conflict,
    #[error("directory not empty")]
    DirectoryNotEmpty,
    #[error("integrity error")]
    Integrity,
    #[error("queue full")]
    QueueFull,
    #[error("unauthorized")]
    Unauthorized,
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl From<ApiErrorKind> for ErrorCode {
    fn from(kind: ApiErrorKind) -> Self {
        match kind {
            ApiErrorKind::NotFound => ErrorCode::NotFound,
            ApiErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
            ApiErrorKind::InvalidName => ErrorCode::InvalidName,
            ApiErrorKind::Conflict => ErrorCode::Conflict,
            ApiErrorKind::DirectoryNotEmpty => ErrorCode::DirectoryNotEmpty,
            ApiErrorKind::Integrity => ErrorCode::Integrity,
            ApiErrorKind::QueueFull => ErrorCode::QueueFull,
            ApiErrorKind::Unauthorized => ErrorCode::Unauthorized,
            ApiErrorKind::BackendUnavailable => ErrorCode::BackendUnavailable,
            ApiErrorKind::InvalidRequest(_) => ErrorCode::InvalidRequest,
        }
    }
}
