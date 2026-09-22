//! Node metadata request/response DTOs.

use chrono::{DateTime, Utc};
use dcfs_core::NodeKind;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::NameBytes;

/// Node response DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResponse {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: NameBytes,
    pub kind: NodeKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
    pub ctime: DateTime<Utc>,
    pub current_version_id: Option<Uuid>,
    pub generation: u64,
    /// Raw target bytes, present only on symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<NameBytes>,
}

/// Request to create a new node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNodeRequest {
    pub parent_id: Uuid,
    pub name: NameBytes,
    pub kind: NodeKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Required when `kind` is `Symlink`, rejected otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<NameBytes>,
    pub idempotency_key: Uuid,
}

/// Request to update node attributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchNodeRequest {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub idempotency_key: Uuid,
}

/// Request to rename a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameNodeRequest {
    pub new_parent_id: Uuid,
    pub new_name: NameBytes,
    pub idempotency_key: Uuid,
}

/// Response for listing children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListChildrenResponse {
    pub children: Vec<NodeResponse>,
    pub has_more: bool,
}

/// Query parameters for resolving a path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveQuery {
    pub path: String,
}

/// What a client needs to know about how the server stores files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsInfoResponse {
    /// Part size for new files. A client that aligns its writes to this lets
    /// the server replace whole parts instead of merging into them.
    pub chunk_size: u64,
}

/// Query parameters for listing children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListChildrenQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}
