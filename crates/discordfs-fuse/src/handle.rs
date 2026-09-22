//! Handle table for tracking open files.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tracks open file handles.
pub struct HandleTable {
    next_handle: AtomicU64,
    handles: Mutex<HashMap<u64, HandleInfo>>,
}

/// Information about an open file handle.
#[derive(Debug, Clone)]
pub struct HandleInfo {
    /// Inode this handle refers to.
    pub ino: u64,
    /// Whether the file is open for writing.
    pub writable: bool,
}

impl HandleTable {
    /// Create a new empty handle table.
    pub fn new() -> Self {
        Self {
            next_handle: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }

    /// Open a new handle for a node.
    pub fn open(&self, ino: u64, writable: bool) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let info = HandleInfo { ino, writable };
        self.handles.lock().insert(handle, info);
        handle
    }

    /// Get information about a handle.
    pub fn get(&self, handle: u64) -> Option<HandleInfo> {
        self.handles.lock().get(&handle).cloned()
    }

    /// Close a handle.
    pub fn close(&self, handle: u64) -> Option<HandleInfo> {
        self.handles.lock().remove(&handle)
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_get_handle() {
        let table = HandleTable::new();
        let node_ino = 42;
        let handle = table.open(node_ino, true);

        let info = table.get(handle).unwrap();
        assert_eq!(info.ino, node_ino);
        assert!(info.writable);
    }

    #[test]
    fn close_removes_handle() {
        let table = HandleTable::new();
        let node_ino = 42;
        let handle = table.open(node_ino, false);

        assert!(table.get(handle).is_some());
        table.close(handle);
        assert!(table.get(handle).is_none());
    }

    #[test]
    fn multiple_handles_unique() {
        let table = HandleTable::new();
        let node_ino = 42;

        let h1 = table.open(node_ino, false);
        let h2 = table.open(node_ino, false);

        assert_ne!(h1, h2);
    }
}
