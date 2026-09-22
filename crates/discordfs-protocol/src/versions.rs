//! Version staging, chunk upload, and commit DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request to stage a new file version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageVersionRequest {
    pub node_id: Uuid,
    pub expected_generation: u64,
    pub expected_current_version: Option<Uuid>,
    pub idempotency_key: Uuid,
}

/// Response after staging a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageVersionResponse {
    pub version_id: Uuid,
    pub node_id: Uuid,
    pub generation: u64,
    pub chunk_size: u64,
}

/// Request to upload a chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadChunkRequest {
    pub chunk_index: u64,
    pub plaintext_size: u64,
    pub plaintext_hash: String,
    pub idempotency_key: Uuid,
}

/// Response after uploading a chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadChunkResponse {
    pub object_id: Uuid,
    pub ciphertext_size: u64,
}

/// Request to commit a staged version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitVersionRequest {
    pub version_id: Uuid,
    pub total_size: u64,
    pub plaintext_hash: String,
    pub expected_generation: u64,
    pub expected_current_version: Option<Uuid>,
    pub idempotency_key: Uuid,
}

/// Response after committing a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitVersionResponse {
    pub version_id: Uuid,
    pub node_id: Uuid,
    pub generation: u64,
    pub committed_at: DateTime<Utc>,
}

/// Chunk metadata in a version manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifestEntry {
    pub chunk_index: u64,
    pub object_id: Uuid,
    pub plaintext_hash: String,
    pub plaintext_size: u64,
}

/// Response for reading a chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadChunkResponse {
    pub object_id: Uuid,
    pub plaintext_hash: String,
    pub plaintext_size: u64,
}
