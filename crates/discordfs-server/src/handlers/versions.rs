//! Version staging, chunk upload, and commit handlers.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use discordfs_db::CommitGuard;
use discordfs_protocol::{
    ChunkManifestEntry, CommitVersionRequest, CommitVersionResponse, StageVersionRequest,
    StageVersionResponse, UploadChunkRequest, UploadChunkResponse,
};
use uuid::Uuid;

use crate::{error::AppError, handlers::data::write_chunk_plaintext, state::AppState};

/// Stage a new file version.
pub async fn stage_version(
    State(state): State<AppState>,
    Json(req): Json<StageVersionRequest>,
) -> Result<(StatusCode, Json<StageVersionResponse>), AppError> {
    // Get the node to verify it exists and get current version
    let node = state.repo.get_node(req.node_id).await?;

    // Create staging version
    let version = state
        .repo
        .create_staging_version(
            req.node_id,
            node.current_version_id,
            state.chunk_size as i64,
        )
        .await?;

    let response = StageVersionResponse {
        version_id: version.id,
        node_id: req.node_id,
        generation: node.generation as u64 + 1,
        chunk_size: version.chunk_size as u64,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// Upload one chunk: the plaintext is the request body, its metadata the query
/// string. The bytes are hashed, encrypted and stored before any metadata is
/// attached, so the manifest never points at an object that does not exist.
pub async fn upload_chunk(
    State(state): State<AppState>,
    Path(version_id): Path<Uuid>,
    Query(req): Query<UploadChunkRequest>,
    body: Bytes,
) -> Result<(StatusCode, Json<UploadChunkResponse>), AppError> {
    let version = state.repo.get_version(version_id).await?;
    if version.state != "staging" {
        return Err(AppError::conflict("version is no longer staging"));
    }

    // The client's claims are checked against the bytes that actually arrived.
    if req.plaintext_size != body.len() as u64 {
        return Err(AppError::bad_request(format!(
            "plaintext_size {} does not match the {} bytes received",
            req.plaintext_size,
            body.len()
        )));
    }
    let chunk_size = version.chunk_size.max(1) as u64;
    if body.len() as u64 > chunk_size {
        return Err(AppError::bad_request(format!(
            "chunk is larger than the version's chunk size of {chunk_size}"
        )));
    }

    let (object_id, plaintext_hash) = write_chunk_plaintext(&state, &body, version.node_id).await?;
    if !req.plaintext_hash.is_empty() && req.plaintext_hash != plaintext_hash {
        return Err(AppError::bad_request(
            "plaintext_hash does not match the body",
        ));
    }

    state
        .repo
        .attach_chunk(
            version_id,
            req.chunk_index as i64,
            (req.chunk_index * chunk_size) as i64,
            body.len() as i32,
            &plaintext_hash,
            object_id,
        )
        .await?;

    let response = UploadChunkResponse {
        object_id,
        ciphertext_size: body.len() as u64
            + discordfs_crypto::ciphertext_overhead(body.len() as u64),
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// List chunks for a version.
pub async fn list_chunks(
    State(state): State<AppState>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<Vec<ChunkManifestEntry>>, AppError> {
    let chunks = state.repo.get_chunks(version_id).await?;
    let entries = chunks
        .into_iter()
        .map(|c| ChunkManifestEntry {
            chunk_index: c.chunk_index as u64,
            object_id: c.object_id,
            plaintext_hash: c.plaintext_hash,
            plaintext_size: c.plaintext_size as u64,
        })
        .collect();
    Ok(Json(entries))
}

/// Commit a staged version.
pub async fn commit_version(
    State(state): State<AppState>,
    Path(version_id): Path<Uuid>,
    Json(req): Json<CommitVersionRequest>,
) -> Result<Json<CommitVersionResponse>, AppError> {
    // Get version to find node_id
    let version = state.repo.get_version(version_id).await?;

    // A version only becomes visible if its manifest describes the whole file:
    // contiguous chunk indexes starting at 0 whose sizes add up to total_size.
    let chunks = state.repo.get_chunks(version_id).await?;
    let mut total: u64 = 0;
    for (expected_index, chunk) in chunks.iter().enumerate() {
        if chunk.chunk_index as usize != expected_index {
            return Err(AppError::bad_request(format!(
                "manifest has a hole: expected chunk {expected_index}, found {}",
                chunk.chunk_index
            )));
        }
        total += chunk.plaintext_size.max(0) as u64;
    }
    if total != req.total_size {
        return Err(AppError::bad_request(format!(
            "manifest covers {total} bytes but total_size is {}",
            req.total_size
        )));
    }

    // Create commit guard
    let guard = CommitGuard {
        node_id: version.node_id,
        version_id,
        expected_generation: req.expected_generation as i64,
        expected_current_version: req.expected_current_version,
    };

    // Commit the version
    let committed = state
        .repo
        .commit_version(guard, req.total_size as i64, &req.plaintext_hash)
        .await?;

    let response = CommitVersionResponse {
        version_id: committed.id,
        node_id: version.node_id,
        generation: req.expected_generation + 1,
        committed_at: committed.committed_at.unwrap_or_else(chrono::Utc::now),
    };

    Ok(Json(response))
}

/// Get a version by ID.
pub async fn get_version(
    State(state): State<AppState>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<discordfs_protocol::CommitVersionResponse>, AppError> {
    let version = state.repo.get_version(version_id).await?;
    let response = discordfs_protocol::CommitVersionResponse {
        version_id: version.id,
        node_id: version.node_id,
        generation: 0, // Not tracked in version record
        committed_at: version.committed_at.unwrap_or(version.created_at),
    };
    Ok(Json(response))
}
