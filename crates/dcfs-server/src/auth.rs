//! Bearer-token authentication.
//!
//! v0.1 has one shared token: the server and whoever mounts the filesystem hold
//! the same secret. It is not a user system — there is exactly one namespace —
//! but it keeps an unauthenticated process on the same host from reading or
//! deleting every file.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use dcfs_protocol::{ApiError, ErrorCode};

use crate::state::AppState;

/// Hash a token for storage and lookup.
///
/// Sessions are found by this, never by the token itself, so a copy of the
/// table is not a set of working credentials.
pub fn hash_token(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// The token out of an Authorization header, however it was presented.
///
/// WebDAV clients — Finder, Windows Explorer, gvfs — speak Basic and have no
/// way to send a bearer token, so the token is accepted as the password of a
/// Basic credential too. The username is ignored: there is one namespace and
/// no user to identify.
pub fn presented_token_for_test(header_value: &str) -> Option<String> {
    presented_token(header_value)
}

fn presented_token(header_value: &str) -> Option<String> {
    if let Some(token) = header_value.strip_prefix("Bearer ") {
        return Some(token.to_string());
    }
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded = base64_decode(encoded)?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_user, password) = decoded.split_once(':')?;
    Some(password.to_string())
}

/// Just enough base64 to read a Basic credential.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// Compare two secrets without leaking their contents through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Length is not secret, but the comparison below must still touch every
    // byte of the longer input rather than stopping at the first mismatch.
    let mut different = (a.len() ^ b.len()) as u8;
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        different |= left ^ right;
    }
    different == 0
}

/// Reject any request that does not carry the configured bearer token.
///
/// Health endpoints are mounted outside this layer so orchestrators can probe
/// them without holding the secret.
pub async fn require_token(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_ref() else {
        // No token configured: the binary refuses to start in that state, so
        // this only happens in tests, which build the state directly.
        return next.run(request).await;
    };

    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let presented = presented_token(header_value);
    let presented = presented.as_deref().unwrap_or("");

    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return next.run(request).await;
    }

    // Not the bootstrap token: it may still be a session issued from it.
    // Sessions are looked up by hash, so an unknown token reveals nothing.
    if !presented.is_empty() {
        if let Ok(session) = state.repo.find_session(&hash_token(presented)).await {
            request.extensions_mut().insert(session.id);
            return next.run(request).await;
        }
    }

    // Say nothing about which part was wrong, and never echo what was sent.
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new(ErrorCode::Unauthorized, "unauthorized")),
    )
        .into_response()
}

/// Reject anything but the bootstrap token.
///
/// Issuing and revoking credentials is the one thing a session must not be
/// able to do: otherwise a leaked session mints replacements for itself.
pub async fn require_bootstrap_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_ref() else {
        return next.run(request).await;
    };
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let presented = presented_token(header_value);
    let presented = presented.as_deref().unwrap_or("");

    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new(
            ErrorCode::Unauthorized,
            "this endpoint needs the bootstrap token",
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn hashing_a_token_does_not_keep_it() {
        use super::hash_token;
        let hash = hash_token("a-real-token");
        assert_ne!(hash, "a-real-token");
        assert!(!hash.contains("a-real-token"));
        assert_eq!(hash, hash_token("a-real-token"), "lookup needs it stable");
        assert_ne!(hash, hash_token("a-real-tokeN"));
    }

    #[test]
    fn compares_by_value_not_by_prefix() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"secret"));
        assert!(constant_time_eq(b"", b""));
    }
}
