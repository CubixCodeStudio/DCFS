//! Cache manager with LRU eviction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::CacheEntry;

/// Cache manager with byte budget and LRU eviction.
pub struct CacheManager {
    entries: HashMap<Uuid, CacheEntry>,
    byte_budget: u64,
    current_bytes: u64,
    cache_dir: PathBuf,
}

impl CacheManager {
    pub fn new(cache_dir: PathBuf, byte_budget: u64) -> Self {
        Self {
            entries: HashMap::new(),
            byte_budget,
            current_bytes: 0,
            cache_dir,
        }
    }

    /// Get or create a cache entry for a node.
    pub fn get_or_create(
        &mut self,
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        generation: u64,
        size: u64,
    ) -> &mut CacheEntry {
        if !self.entries.contains_key(&node_id) {
            let path = self.cache_dir.join(node_id.to_string());
            let entry = CacheEntry::new(node_id, base_version_id, generation, path, size);
            self.current_bytes += size;
            self.entries.insert(node_id, entry);
        }
        let entry = self.entries.get_mut(&node_id).unwrap();
        entry.touch();
        entry
    }

    /// Get a cache entry.
    pub fn get(&mut self, node_id: &Uuid) -> Option<&mut CacheEntry> {
        let entry = self.entries.get_mut(node_id)?;
        entry.touch();
        Some(entry)
    }

    /// Remove a cache entry (only if clean).
    pub fn remove(&mut self, node_id: &Uuid) -> bool {
        if let Some(entry) = self.entries.get(node_id) {
            if !entry.is_evictable() {
                return false;
            }
        }
        if let Some(entry) = self.entries.remove(node_id) {
            self.current_bytes = self.current_bytes.saturating_sub(entry.logical_size);
            true
        } else {
            false
        }
    }

    /// Evict clean entries to stay within budget.
    pub fn evict_if_needed(&mut self) {
        while self.current_bytes > self.byte_budget {
            // Find oldest clean entry
            let oldest = self
                .entries
                .iter()
                .filter(|(_, e)| e.is_evictable())
                .min_by_key(|(_, e)| e.last_access)
                .map(|(id, _)| *id);

            if let Some(id) = oldest {
                if let Some(entry) = self.entries.remove(&id) {
                    self.current_bytes = self.current_bytes.saturating_sub(entry.logical_size);
                }
            } else {
                break; // No more evictable entries
            }
        }
    }

    /// Get all dirty entries (for recovery).
    pub fn dirty_entries(&self) -> Vec<&CacheEntry> {
        self.entries.values().filter(|e| e.is_dirty()).collect()
    }

    /// Get all entries.
    pub fn entries(&self) -> &HashMap<Uuid, CacheEntry> {
        &self.entries
    }

    /// Get cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Get current byte count.
    pub fn current_bytes(&self) -> u64 {
        self.current_bytes
    }

    /// Get byte budget.
    pub fn byte_budget(&self) -> u64 {
        self.byte_budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn get_or_create_creates_entry() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 1024 * 1024);
        let node_id = Uuid::new_v4();
        let entry = mgr.get_or_create(node_id, None, 0, 100);
        assert_eq!(entry.node_id, node_id);
        assert_eq!(entry.logical_size, 100);
    }

    #[test]
    fn dirty_entries_not_evicted() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 50);
        let node_id = Uuid::new_v4();
        let entry = mgr.get_or_create(node_id, None, 0, 100);
        entry.mark_dirty(0..100);

        mgr.evict_if_needed();

        assert!(mgr.entries.contains_key(&node_id));
    }

    #[test]
    fn clean_entries_evicted() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 50);
        let node_id = Uuid::new_v4();
        mgr.get_or_create(node_id, None, 0, 100);

        mgr.evict_if_needed();

        assert!(!mgr.entries.contains_key(&node_id));
    }

    #[test]
    fn committing_entries_not_evicted() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 50);
        let node_id = Uuid::new_v4();
        let entry = mgr.get_or_create(node_id, None, 0, 100);
        entry.mark_dirty(0..100);
        entry.mark_committing();

        mgr.evict_if_needed();

        assert!(mgr.entries.contains_key(&node_id));
    }

    #[test]
    fn remove_dirty_entry_fails() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();
        let entry = mgr.get_or_create(node_id, None, 0, 100);
        entry.mark_dirty(0..100);

        let removed = mgr.remove(&node_id);

        assert!(!removed);
        assert!(mgr.entries.contains_key(&node_id));
    }

    #[test]
    fn remove_clean_entry_succeeds() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();
        mgr.get_or_create(node_id, None, 0, 100);

        let removed = mgr.remove(&node_id);

        assert!(removed);
        assert!(!mgr.entries.contains_key(&node_id));
    }

    #[test]
    fn get_or_create_returns_existing() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node_id = Uuid::new_v4();
        let entry1 = mgr.get_or_create(node_id, None, 0, 100);
        entry1.mark_dirty(0..50);
        let size1 = entry1.logical_size;

        let entry2 = mgr.get_or_create(node_id, None, 0, 200);
        assert_eq!(entry2.logical_size, size1); // Should not change
        assert!(entry2.is_dirty());
    }

    #[test]
    fn current_bytes_tracked() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 1024);
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        mgr.get_or_create(node1, None, 0, 100);
        assert_eq!(mgr.current_bytes, 100);

        mgr.get_or_create(node2, None, 0, 200);
        assert_eq!(mgr.current_bytes, 300);

        mgr.remove(&node1);
        assert_eq!(mgr.current_bytes, 200);
    }

    #[test]
    fn evict_multiple_entries() {
        let mut mgr = CacheManager::new(PathBuf::from("/tmp/cache"), 100);
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();
        let node3 = Uuid::new_v4();

        mgr.get_or_create(node1, None, 0, 50);
        std::thread::sleep(std::time::Duration::from_millis(10));
        mgr.get_or_create(node2, None, 0, 50);
        std::thread::sleep(std::time::Duration::from_millis(10));
        mgr.get_or_create(node3, None, 0, 50);

        mgr.evict_if_needed();

        // Should evict oldest entries first
        assert!(mgr.current_bytes <= 100);
    }
}
