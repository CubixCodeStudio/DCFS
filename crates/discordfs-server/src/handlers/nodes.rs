//! Node CRUD handlers.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use discordfs_core::NodeKind;
use discordfs_protocol::{
    CreateNodeRequest, ListChildrenQuery, ListChildrenResponse, NodeResponse, PatchNodeRequest,
    RenameNodeRequest, ResolveQuery,
};
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

/// Convert a NodeRecord to NodeResponse.
fn node_to_response(record: discordfs_db::NodeRecord) -> NodeResponse {
    let kind = match record.kind.as_str() {
        "directory" => NodeKind::Directory,
        "symlink" => NodeKind::Symlink,
        _ => NodeKind::File,
    };
    NodeResponse {
        id: record.id,
        parent_id: record.parent_id,
        name: discordfs_protocol::NameBytes(record.name),
        kind,
        mode: record.mode as u32,
        uid: record.uid as u32,
        gid: record.gid as u32,
        size: record.size as u64,
        atime: record.atime,
        mtime: record.mtime,
        ctime: record.ctime,
        current_version_id: record.current_version_id,
        generation: record.generation as u64,
        link_target: record.link_target.map(discordfs_protocol::NameBytes),
    }
}

/// Report how the server chunks files.
pub async fn get_fs_info(
    State(state): State<AppState>,
) -> Json<discordfs_protocol::FsInfoResponse> {
    Json(discordfs_protocol::FsInfoResponse {
        chunk_size: state.chunk_size,
    })
}

/// Get the root node.
pub async fn get_root(State(state): State<AppState>) -> Result<Json<NodeResponse>, AppError> {
    let root = state.repo.get_root().await?;
    Ok(Json(node_to_response(root)))
}

/// Get a node by ID.
pub async fn get_node(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<NodeResponse>, AppError> {
    let node = state.repo.get_node(id).await?;
    Ok(Json(node_to_response(node)))
}

/// Delete a node.
pub async fn delete_node(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let node = state.repo.get_node(id).await?;
    if node.kind == "directory" {
        // rmdir(2) must fail on a non-empty directory rather than orphaning
        // everything under it.
        if !state.repo.list_children(id, 1, 0).await?.is_empty() {
            return Err(AppError::directory_not_empty());
        }
        if node.parent_id.is_none() {
            return Err(AppError::bad_request("cannot delete the root directory"));
        }
    }
    state.repo.delete_node(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Update node attributes.
pub async fn patch_node(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchNodeRequest>,
) -> Result<Json<NodeResponse>, AppError> {
    let node = state
        .repo
        .update_node_attr(
            id,
            req.mode.map(|m| m as i32),
            req.uid.map(|u| u as i32),
            req.gid.map(|g| g as i32),
            req.size.map(|s| s as i64),
            req.mtime,
            req.atime,
        )
        .await?;
    Ok(Json(node_to_response(node)))
}

/// Rename a node.
pub async fn rename_node(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameNodeRequest>,
) -> Result<Json<NodeResponse>, AppError> {
    let new_name = req.new_name.to_name()?;

    // rename(2) replaces the destination, but only between like kinds: a
    // directory cannot land on a file, or a file on a directory.
    let source = state.repo.get_node(id).await?;
    let existing = state
        .repo
        .list_children(req.new_parent_id, 1000, 0)
        .await?
        .into_iter()
        .find(|c| c.name == new_name.as_bytes());
    if let Some(target) = existing {
        if target.id != id && target.kind != source.kind {
            return Err(AppError::bad_request(format!(
                "cannot rename a {} onto a {}",
                source.kind, target.kind
            )));
        }
    }

    state
        .repo
        .rename_node(id, req.new_parent_id, new_name.into_bytes())
        .await?;
    let node = state.repo.get_node(id).await?;
    Ok(Json(node_to_response(node)))
}

/// Resolve one name inside a directory.
///
/// `name` is unpadded base64url, like every other filename on the wire, which
/// also keeps raw bytes and `/` out of the path.
pub async fn lookup_child(
    State(state): State<AppState>,
    Path((parent_id, name)): Path<(Uuid, String)>,
) -> Result<Json<NodeResponse>, AppError> {
    let name = discordfs_protocol::NameBytes::from_base64(&name)
        .map_err(|e| AppError::bad_request(format!("name is not base64url: {e}")))?;
    let node = state.repo.find_child(parent_id, name.as_bytes()).await?;
    Ok(Json(node_to_response(node)))
}

/// List children of a directory.
pub async fn list_children(
    State(state): State<AppState>,
    Path(parent_id): Path<Uuid>,
    Query(query): Query<ListChildrenQuery>,
) -> Result<Json<ListChildrenResponse>, AppError> {
    let limit = query.limit.unwrap_or(100);
    let offset = query.offset.unwrap_or(0);
    let children = state.repo.list_children(parent_id, limit, offset).await?;
    let has_more = children.len() == limit as usize;
    let response = ListChildrenResponse {
        children: children.into_iter().map(node_to_response).collect(),
        has_more,
    };
    Ok(Json(response))
}

/// Create a new node.
pub async fn create_node(
    State(state): State<AppState>,
    Json(req): Json<CreateNodeRequest>,
) -> Result<(StatusCode, Json<NodeResponse>), AppError> {
    let name = req.name.to_name()?;

    // A symlink carries its target instead of content, so it takes a different
    // path: no mode, no versions, no chunks.
    if req.kind == NodeKind::Symlink {
        let Some(target) = req.link_target else {
            return Err(AppError::bad_request("a symlink needs link_target"));
        };
        if target.as_bytes().is_empty() {
            return Err(AppError::bad_request("a symlink target cannot be empty"));
        }
        let node = state
            .repo
            .create_symlink(
                req.idempotency_key,
                req.parent_id,
                name.into_bytes(),
                target.as_bytes().to_vec(),
                req.uid as i32,
                req.gid as i32,
            )
            .await?;
        return Ok((StatusCode::CREATED, Json(node_to_response(node))));
    }
    if req.link_target.is_some() {
        return Err(AppError::bad_request(
            "link_target is only valid when kind is Symlink",
        ));
    }

    let kind_str = match req.kind {
        NodeKind::Directory => "directory",
        NodeKind::File => "file",
        NodeKind::Symlink => unreachable!("handled above"),
    };
    let node = state
        .repo
        .create_node(
            req.idempotency_key,
            req.parent_id,
            name.into_bytes(),
            kind_str,
            req.mode as i32,
            req.uid as i32,
            req.gid as i32,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(node_to_response(node))))
}

/// Resolve a path to a node.
pub async fn resolve_path(
    State(state): State<AppState>,
    Query(query): Query<ResolveQuery>,
) -> Result<Json<NodeResponse>, AppError> {
    let path = query.path.trim_start_matches('/');
    if path.is_empty() {
        let root = state.repo.get_root().await?;
        return Ok(Json(node_to_response(root)));
    }

    let mut current = state.repo.get_root().await?;
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let children = state.repo.list_children(current.id, 1000, 0).await?;
        current = children
            .into_iter()
            .find(|c| c.name == segment.as_bytes())
            .ok_or_else(|| AppError::not_found("path not found"))?;
    }
    Ok(Json(node_to_response(current)))
}
