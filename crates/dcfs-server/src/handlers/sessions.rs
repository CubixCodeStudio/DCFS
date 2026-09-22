//! Short-lived credentials, issued against the bootstrap token.
//!
//! The bootstrap `API_TOKEN` is the one secret an operator holds and never
//! rotates casually. Handing it to every mount means every mount holds a
//! credential that cannot be revoked without restarting the server, so a mount
//! exchanges it once for a session that expires on its own and can be revoked
//! individually.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{auth::hash_token, error::AppError, state::AppState};

/// Longest life a session may be given.
const MAX_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Default, Deserialize)]
pub struct CreateSessionRequest {
    /// How long the session should last. Clamped to 30 days.
    pub ttl_secs: Option<i64>,
    /// Free text to tell sessions apart in an audit, such as a host name.
    /// Never a secret: it is stored and logged as-is.
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub id: Uuid,
    /// The credential. Returned once, never stored, never recoverable.
    pub token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

/// `POST /api/v1/sessions` — only the bootstrap token may call this.
pub async fn create_session(
    State(state): State<AppState>,
    body: Option<Json<CreateSessionRequest>>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), AppError> {
    let request = body.map(|Json(body)| body).unwrap_or_default();
    let ttl = request
        .ttl_secs
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS);
    let label = request.label.unwrap_or_default();
    if label.len() > 200 {
        return Err(AppError::bad_request("label is too long"));
    }

    // 32 bytes from the OS: the token is the only thing standing between a
    // caller and every file, so it is not derived from anything guessable.
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AppError::internal(format!("cannot generate a token: {e}")))?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::seconds(ttl);
    let session = state
        .repo
        .create_session(id, &hash_token(&token), &label, expires_at)
        .await?;

    tracing::info!(session = %session.id, label = %session.label, "session issued");
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            id: session.id,
            token,
            expires_at: session.expires_at,
        }),
    ))
}

/// `DELETE /api/v1/sessions/:id` — only the bootstrap token may call this.
pub async fn revoke_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    if state.repo.revoke_session(id).await? {
        tracing::info!(session = %id, "session revoked");
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Already revoked or never existed: the end state is the same, and
        // saying which would tell a caller whether an id is real.
        Ok(StatusCode::NO_CONTENT)
    }
}
