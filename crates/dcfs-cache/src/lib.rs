//! Local write-back cache for DCFS.

pub mod dirty;
pub mod entry;
pub mod journal;
pub mod manager;

pub use dirty::*;
pub use entry::*;
pub use journal::*;
pub use manager::*;

use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// Thread-safe cache manager wrapper using RwLock.
#[derive(Clone)]
pub struct ThreadSafeCacheManager {
    inner: Arc<RwLock<CacheManager>>,
}

impl ThreadSafeCacheManager {
    pub fn new(cache_dir: PathBuf, byte_budget: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CacheManager::new(cache_dir, byte_budget))),
        }
    }

    /// Get or create a cache entry (read-write lock).
    pub fn get_or_create(
        &self,
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        generation: u64,
        size: u64,
    ) -> CacheEntry {
        let mut mgr = self.inner.write();
        mgr.get_or_create(node_id, base_version_id, generation, size)
            .clone()
    }

    /// Get a cache entry (read-write lock).
    pub fn get(&self, node_id: &Uuid) -> Option<CacheEntry> {
        let mut mgr = self.inner.write();
        mgr.get(node_id).map(|e| e.clone())
    }

    /// Remove a cache entry (read-write lock).
    pub fn remove(&self, node_id: &Uuid) -> bool {
        let mut mgr = self.inner.write();
        mgr.remove(node_id)
    }

    /// Evict clean entries if needed (read-write lock).
    pub fn evict_if_needed(&self) {
        let mut mgr = self.inner.write();
        mgr.evict_if_needed();
    }

    /// Get all dirty entries (read lock).
    pub fn dirty_entries(&self) -> Vec<CacheEntry> {
        let mgr = self.inner.read();
        mgr.dirty_entries().into_iter().cloned().collect()
    }

    /// Get cache directory (read lock).
    pub fn cache_dir(&self) -> PathBuf {
        let mgr = self.inner.read();
        mgr.cache_dir().to_path_buf()
    }

    /// Get current byte count (read lock).
    pub fn current_bytes(&self) -> u64 {
        let mgr = self.inner.read();
        mgr.current_bytes()
    }

    /// Get byte budget (read lock).
    pub fn byte_budget(&self) -> u64 {
        let mgr = self.inner.read();
        mgr.byte_budget()
    }

    /// Mark a range as dirty for a node (read-write lock).
    pub fn mark_dirty(&self, node_id: &Uuid, range: std::ops::Range<u64>) -> bool {
        let mut mgr = self.inner.write();
        if let Some(entry) = mgr.get(node_id) {
            entry.mark_dirty(range);
            true
        } else {
            false
        }
    }

    /// Mark an entry as clean (read-write lock).
    pub fn mark_clean(&self, node_id: &Uuid) -> bool {
        let mut mgr = self.inner.write();
        if let Some(entry) = mgr.get(node_id) {
            entry.mark_clean();
            true
        } else {
            false
        }
    }

    /// Mark an entry as committing (read-write lock).
    pub fn mark_committing(&self, node_id: &Uuid) -> bool {
        let mut mgr = self.inner.write();
        if let Some(entry) = mgr.get(node_id) {
            entry.mark_committing()
        } else {
            false
        }
    }

    /// Mark an entry as conflicted (read-write lock).
    pub fn mark_conflicted(&self, node_id: &Uuid) -> bool {
        let mut mgr = self.inner.write();
        if let Some(entry) = mgr.get(node_id) {
            entry.mark_conflicted();
            true
        } else {
            false
        }
    }

    /// Access the inner manager with a closure (for complex operations).
    pub fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CacheManager) -> R,
    {
        let mut mgr = self.inner.write();
        f(&mut mgr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn thread_safe_basic_operations() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();

        let entry = mgr.get_or_create(node_id, None, 0, 100);
        assert_eq!(entry.node_id, node_id);
        assert_eq!(entry.logical_size, 100);

        assert!(mgr.mark_dirty(&node_id, 0..50));
        let dirty = mgr.dirty_entries();
        assert_eq!(dirty.len(), 1);

        assert!(mgr.mark_clean(&node_id));
        let dirty = mgr.dirty_entries();
        assert!(dirty.is_empty());
    }

    #[test]
    fn thread_safe_concurrent_access() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 1024 * 1024);
        let mut handles = vec![];

        // Spawn multiple threads that create and modify entries
        for i in 0..10 {
            let mgr_clone = mgr.clone();
            let handle = thread::spawn(move || {
                let node_id = Uuid::new_v4();
                mgr_clone.get_or_create(node_id, None, 0, 100);
                mgr_clone.mark_dirty(&node_id, 0..50);
                thread::sleep(std::time::Duration::from_millis(i));
                mgr_clone.mark_clean(&node_id);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All entries should be clean now
        let dirty = mgr.dirty_entries();
        assert!(dirty.is_empty());
    }

    #[test]
    fn thread_safe_eviction() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 100);

        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        mgr.get_or_create(node1, None, 0, 60);
        mgr.get_or_create(node2, None, 0, 60);

        assert_eq!(mgr.current_bytes(), 120);

        mgr.evict_if_needed();

        assert!(mgr.current_bytes() <= 100);
    }

    #[test]
    fn thread_safe_state_transitions() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();

        mgr.get_or_create(node_id, None, 0, 100);

        // Clean -> Dirty
        assert!(mgr.mark_dirty(&node_id, 0..50));

        // Dirty -> Committing
        assert!(mgr.mark_committing(&node_id));

        // Committing -> Conflicted
        assert!(mgr.mark_conflicted(&node_id));

        // Conflicted -> Dirty
        assert!(mgr.mark_dirty(&node_id, 0..10));

        // Dirty -> Clean
        assert!(mgr.mark_clean(&node_id));
    }

    #[test]
    fn thread_safe_remove() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();

        mgr.get_or_create(node_id, None, 0, 100);
        assert!(mgr.remove(&node_id));
        assert!(mgr.get(&node_id).is_none());
    }

    #[test]
    fn thread_safe_remove_dirty_fails() {
        let mgr = ThreadSafeCacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();

        mgr.get_or_create(node_id, None, 0, 100);
        mgr.mark_dirty(&node_id, 0..50);

        assert!(!mgr.remove(&node_id));
        assert!(mgr.get(&node_id).is_some());
    }
}
