//! Fake client implementation for testing.

use crate::client::{ClientError, ServerClient};
use async_trait::async_trait;
use chrono::Utc;
use discordfs_core::NodeKind;
use discordfs_protocol::{
    CreateNodeRequest, ListChildrenResponse, NameBytes, NodeResponse, PatchNodeRequest,
    RenameNodeRequest,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use uuid::Uuid;

/// In-memory fake client for testing.
pub struct FakeClient {
    nodes: Mutex<HashMap<Uuid, NodeData>>,
    file_data: Mutex<HashMap<Uuid, Vec<u8>>>,
}

struct NodeData {
    node: NodeResponse,
}

impl FakeClient {
    /// Small enough that tests cross part boundaries without moving megabytes.
    pub const CHUNK_SIZE: u64 = 64;

    /// Create a new fake client with a root directory.
    pub fn new() -> Self {
        let root_id = Uuid::from_u128(0);
        let now = Utc::now();
        let root = NodeResponse {
            id: root_id,
            parent_id: None,
            name: NameBytes::new(b"root".to_vec()).unwrap(),
            kind: NodeKind::Directory,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            current_version_id: None,
            generation: 0,
            link_target: None,
        };

        let mut nodes = HashMap::new();
        nodes.insert(root_id, NodeData { node: root });

        Self {
            nodes: Mutex::new(nodes),
            file_data: Mutex::new(HashMap::new()),
        }
    }

    /// Get the root node ID.
    pub fn root_id() -> Uuid {
        Uuid::from_u128(0)
    }
}

impl Default for FakeClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ServerClient for FakeClient {
    async fn fs_info(&self) -> Result<discordfs_protocol::FsInfoResponse, ClientError> {
        Ok(discordfs_protocol::FsInfoResponse {
            chunk_size: Self::CHUNK_SIZE,
        })
    }

    async fn get_node(&self, node_id: Uuid) -> Result<NodeResponse, ClientError> {
        let nodes = self.nodes.lock();
        nodes
            .get(&node_id)
            .map(|n| n.node.clone())
            .ok_or(ClientError::NotFound)
    }

    async fn get_node_by_path(&self, path: &str) -> Result<NodeResponse, ClientError> {
        // Simple implementation: only supports "/" for root
        if path == "/" {
            return self.get_node(Self::root_id()).await;
        }
        Err(ClientError::NotFound)
    }

    async fn find_child(&self, parent_id: Uuid, name: &[u8]) -> Result<NodeResponse, ClientError> {
        let nodes = self.nodes.lock();
        if !nodes.contains_key(&parent_id) {
            return Err(ClientError::NotFound);
        }
        nodes
            .values()
            .find(|n| n.node.parent_id == Some(parent_id) && n.node.name.as_bytes() == name)
            .map(|n| n.node.clone())
            .ok_or(ClientError::NotFound)
    }

    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<ListChildrenResponse, ClientError> {
        let nodes = self.nodes.lock();

        // Check parent exists
        if !nodes.contains_key(&parent_id) {
            return Err(ClientError::NotFound);
        }

        // Find all children
        let mut children: Vec<_> = nodes
            .values()
            .filter(|n| n.node.parent_id == Some(parent_id))
            .map(|n| n.node.clone())
            .collect();

        // Sort by name for deterministic order
        children.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

        // Apply offset and limit
        let offset = offset.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(children.len() as u32) as usize;
        let children = children.into_iter().skip(offset).take(limit).collect();

        Ok(ListChildrenResponse {
            children,
            has_more: false,
        })
    }

    async fn create_node(&self, req: CreateNodeRequest) -> Result<NodeResponse, ClientError> {
        let mut nodes = self.nodes.lock();

        // Check parent exists
        if !nodes.contains_key(&req.parent_id) {
            return Err(ClientError::NotFound);
        }

        // Check for duplicate name
        let name_bytes = req.name.as_bytes();
        for node in nodes.values() {
            if node.node.parent_id == Some(req.parent_id) && node.node.name.as_bytes() == name_bytes
            {
                return Err(ClientError::AlreadyExists);
            }
        }

        let now = Utc::now();
        let node_id = Uuid::new_v4();
        let node = NodeResponse {
            id: node_id,
            parent_id: Some(req.parent_id),
            name: req.name,
            kind: req.kind,
            mode: req.mode,
            uid: req.uid,
            gid: req.gid,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            current_version_id: None,
            generation: 0,
            link_target: req
                .link_target
                .clone()
                .map(|t| NameBytes(t.as_bytes().to_vec())),
        };

        nodes.insert(node_id, NodeData { node: node.clone() });

        // Initialize empty file data for files
        if req.kind == NodeKind::File {
            self.file_data.lock().insert(node_id, Vec::new());
        }

        Ok(node)
    }

    async fn patch_node(
        &self,
        node_id: Uuid,
        req: PatchNodeRequest,
    ) -> Result<NodeResponse, ClientError> {
        let mut nodes = self.nodes.lock();
        let data = nodes.get_mut(&node_id).ok_or(ClientError::NotFound)?;

        if let Some(mode) = req.mode {
            data.node.mode = mode;
        }
        if let Some(uid) = req.uid {
            data.node.uid = uid;
        }
        if let Some(gid) = req.gid {
            data.node.gid = gid;
        }
        if let Some(size) = req.size {
            data.node.size = size;
        }
        if let Some(mtime) = req.mtime {
            data.node.mtime = mtime;
        }
        if let Some(atime) = req.atime {
            data.node.atime = atime;
        }

        data.node.ctime = Utc::now();
        data.node.generation += 1;

        Ok(data.node.clone())
    }

    async fn rename_node(
        &self,
        node_id: Uuid,
        req: RenameNodeRequest,
    ) -> Result<NodeResponse, ClientError> {
        let mut nodes = self.nodes.lock();

        // Check node exists
        if !nodes.contains_key(&node_id) {
            return Err(ClientError::NotFound);
        }

        // rename(2) replaces the destination, as the real server does.
        let new_name_bytes = req.new_name.as_bytes().to_vec();
        let victim = nodes
            .values()
            .find(|n| {
                n.node.id != node_id
                    && n.node.parent_id == Some(req.new_parent_id)
                    && n.node.name.as_bytes() == new_name_bytes
            })
            .map(|n| (n.node.id, n.node.kind));
        if let Some((victim_id, kind)) = victim {
            if kind == NodeKind::Directory
                && nodes.values().any(|n| n.node.parent_id == Some(victim_id))
            {
                return Err(ClientError::DirectoryNotEmpty);
            }
            nodes.remove(&victim_id);
            self.file_data.lock().remove(&victim_id);
        }

        let data = nodes.get_mut(&node_id).unwrap();
        data.node.parent_id = Some(req.new_parent_id);
        data.node.name = req.new_name;
        data.node.ctime = Utc::now();
        data.node.generation += 1;

        Ok(data.node.clone())
    }

    async fn delete_node(&self, node_id: Uuid) -> Result<(), ClientError> {
        let mut nodes = self.nodes.lock();

        // Check node exists
        if !nodes.contains_key(&node_id) {
            return Err(ClientError::NotFound);
        }

        // Check if directory is empty
        let node = nodes.get(&node_id).unwrap();
        if node.node.kind == NodeKind::Directory {
            let has_children = nodes.values().any(|n| n.node.parent_id == Some(node_id));
            if has_children {
                return Err(ClientError::DirectoryNotEmpty);
            }
        }

        nodes.remove(&node_id);
        self.file_data.lock().remove(&node_id);

        Ok(())
    }

    async fn read_file(
        &self,
        node_id: Uuid,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, ClientError> {
        let data = self.file_data.lock();
        let file_data = data.get(&node_id).ok_or(ClientError::NotFound)?;

        let offset = offset as usize;
        let size = size as usize;

        if offset >= file_data.len() {
            return Ok(Vec::new());
        }

        let end = (offset + size).min(file_data.len());
        Ok(file_data[offset..end].to_vec())
    }

    async fn write_file(
        &self,
        node_id: Uuid,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, ClientError> {
        let mut file_data = self.file_data.lock();
        let file = file_data.get_mut(&node_id).ok_or(ClientError::NotFound)?;

        let offset = offset as usize;
        let end = offset + data.len();

        // Extend file if necessary
        if end > file.len() {
            file.resize(end, 0);
        }

        // Write data
        file[offset..end].copy_from_slice(data);

        // Update node size, and publish a new version like the real server's
        // commit does — clients key their caches on it.
        let mut nodes = self.nodes.lock();
        if let Some(node_data) = nodes.get_mut(&node_id) {
            node_data.node.size = file.len() as u64;
            node_data.node.mtime = Utc::now();
            node_data.node.current_version_id = Some(Uuid::new_v4());
            node_data.node.generation += 1;
        }

        Ok(data.len() as u64)
    }

    async fn sync_file(&self, node_id: Uuid) -> Result<(), ClientError> {
        // Check node exists
        let nodes = self.nodes.lock();
        if !nodes.contains_key(&node_id) {
            return Err(ClientError::NotFound);
        }
        Ok(())
    }
}
