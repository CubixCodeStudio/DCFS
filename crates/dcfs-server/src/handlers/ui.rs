//! The browser UI: signing in, and the page itself.
//!
//! Signing in exchanges the API token for a session, and the session goes
//! back as an HttpOnly cookie. No script on the page can read it and nothing
//! is kept in browser storage, so a cross-site script or a shared machine does
//! not walk away with a credential that opens every file.

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{hash_token, SESSION_COOKIE};
use crate::{error::AppError, state::AppState};

/// One page, no build step. It talks to the same API as every other client.
const PAGE: &str = include_str!("../ui/index.html");

/// How long a browser session lasts before it has to sign in again.
const SESSION_SECS: i64 = 12 * 3600;

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

/// `POST /ui/login` — trade the API token for a session cookie.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, AppError> {
    let Some(expected) = state.api_token.as_ref() else {
        return Err(AppError::internal("no API token is configured"));
    };
    if !crate::auth::token_matches(&request.token, expected) {
        // Nothing about which part was wrong, and a pause so the answer cannot
        // be timed apart from a slow network.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        return Ok((StatusCode::UNAUTHORIZED, "wrong token").into_response());
    }

    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AppError::internal(format!("cannot generate a token: {e}")))?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let session = state
        .repo
        .create_session(
            Uuid::new_v4(),
            &hash_token(&token),
            "browser",
            Utc::now() + Duration::seconds(SESSION_SECS),
        )
        .await?;
    tracing::info!(session = %session.id, "browser signed in");

    // Secure only where it can be honoured: a Secure cookie is dropped over
    // plain HTTP, which would leave the UI unable to sign in at all on a local
    // run. Behind the TLS profile the proxy sets x-forwarded-proto.
    let over_tls = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|proto| proto.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_SECS}{}",
        if over_tls { "; Secure" } else { "" }
    );

    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

/// `POST /ui/logout` — revoke the session and clear the cookie.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::auth::session_cookie)
    {
        // Revoke it server-side as well: clearing the cookie only stops this
        // browser from sending a credential that would otherwise still work.
        if let Ok(session) = state.repo.find_session(&hash_token(&token)).await {
            let _ = state.repo.revoke_session(session.id).await;
        }
    }
    let cleared = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cleared)], StatusCode::NO_CONTENT).into_response()
}
