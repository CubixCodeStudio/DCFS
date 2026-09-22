//! Node types and filesystem attributes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{FileVersionId, NodeId};

/// The kind of filesystem node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// A directory node.
    Directory,
    /// A regular file node.
    File,
    /// A symbolic link. Its target is stored as raw bytes on the node, not as
    /// file content, because a link has no versions and no chunks.
    Symlink,
}

/// Portable subset of POSIX file attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAttr {
    /// File mode (permissions and file type bits).
    pub mode: u32,
    /// User ID of owner.
    pub uid: u32,
    /// Group ID of owner.
    pub gid: u32,
    /// File size in bytes.
    pub size: u64,
    /// Last access time.
    pub atime: DateTime<Utc>,
    /// Last modification time.
    pub mtime: DateTime<Utc>,
    /// Last status change time.
    pub ctime: DateTime<Utc>,
}

impl Default for FileAttr {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }
}

/// A filesystem node (file or directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable identity.
    pub id: NodeId,
    /// Parent node ID (None for root).
    pub parent_id: Option<NodeId>,
    /// Filename as raw bytes.
    pub name: crate::NodeName,
    /// Whether this is a file or directory.
    pub kind: NodeKind,
    /// File attributes.
    pub attr: FileAttr,
    /// Current committed version (files only).
    pub current_version_id: Option<FileVersionId>,
    /// Generation counter for optimistic concurrency.
    pub generation: u64,
}

impl Node {
    /// Create a new directory node.
    pub fn new_directory(
        id: NodeId,
        parent_id: Option<NodeId>,
        name: crate::NodeName,
        attr: FileAttr,
    ) -> Self {
        Self {
            id,
            parent_id,
            name,
            kind: NodeKind::Directory,
            attr: FileAttr {
                mode: attr.mode | 0o40000, // S_IFDIR
                ..attr
            },
            current_version_id: None,
            generation: 0,
        }
    }

    /// Create a new file node.
    pub fn new_file(
        id: NodeId,
        parent_id: Option<NodeId>,
        name: crate::NodeName,
        attr: FileAttr,
    ) -> Self {
        Self {
            id,
            parent_id,
            name,
            kind: NodeKind::File,
            attr: FileAttr {
                mode: attr.mode | 0o100000, // S_IFREG
                ..attr
            },
            current_version_id: None,
            generation: 0,
        }
    }

    /// Check if this node is a directory.
    pub fn is_directory(&self) -> bool {
        self.kind == NodeKind::Directory
    }

    /// Check if this node is a file.
    pub fn is_file(&self) -> bool {
        self.kind == NodeKind::File
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_has_directory_bit() {
        let attr = FileAttr::default();
        let node = Node::new_directory(
            NodeId::new(),
            None,
            crate::NodeName::new(b"test".to_vec()).unwrap(),
            attr,
        );
        assert!(node.attr.mode & 0o40000 != 0);
        assert!(node.is_directory());
    }

    #[test]
    fn file_has_regular_bit() {
        let attr = FileAttr::default();
        let node = Node::new_file(
            NodeId::new(),
            None,
            crate::NodeName::new(b"test".to_vec()).unwrap(),
            attr,
        );
        assert!(node.attr.mode & 0o100000 != 0);
        assert!(node.is_file());
    }

    #[test]
    fn new_node_has_zero_generation() {
        let node = Node::new_file(
            NodeId::new(),
            None,
            crate::NodeName::new(b"test".to_vec()).unwrap(),
            FileAttr::default(),
        );
        assert_eq!(node.generation, 0);
    }
}
