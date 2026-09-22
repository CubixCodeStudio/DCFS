//! Cache entry state machine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::DirtyExtents;

/// State of a cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheState {
    /// Clean: matches remote committed version.
    Clean,
    /// Dirty: has local modifications.
    Dirty,
    /// Committing: being uploaded.
    Committing,
    /// Conflicted: remote changed during local write.
    Conflicted,
}

/// A cache entry for a file.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub node_id: Uuid,
    pub base_version_id: Option<Uuid>,
    pub generation: u64,
    pub local_path: PathBuf,
    pub dirty_extents: DirtyExtents,
    pub logical_size: u64,
    pub last_access: DateTime<Utc>,
    pub state: CacheState,
}

impl CacheEntry {
    pub fn new(
        node_id: Uuid,
        base_version_id: Option<Uuid>,
        generation: u64,
        local_path: PathBuf,
        logical_size: u64,
    ) -> Self {
        Self {
            node_id,
            base_version_id,
            generation,
            local_path,
            dirty_extents: DirtyExtents::new(),
            logical_size,
            last_access: Utc::now(),
            state: CacheState::Clean,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.state == CacheState::Dirty || self.dirty_extents.is_dirty()
    }

    pub fn is_evictable(&self) -> bool {
        self.state == CacheState::Clean && !self.dirty_extents.is_dirty()
    }

    pub fn mark_dirty(&mut self, range: std::ops::Range<u64>) {
        self.dirty_extents.insert(range);
        self.state = CacheState::Dirty;
    }

    pub fn mark_clean(&mut self) {
        self.dirty_extents.clear_all();
        self.state = CacheState::Clean;
    }

    /// Begin committing. Transitions Dirty -> Committing.
    pub fn mark_committing(&mut self) -> bool {
        if self.state == CacheState::Dirty {
            self.state = CacheState::Committing;
            true
        } else {
            false
        }
    }

    pub fn mark_conflicted(&mut self) {
        self.state = CacheState::Conflicted;
    }

    pub fn touch(&mut self) {
        self.last_access = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn new_entry_is_clean() {
        let entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        assert_eq!(entry.state, CacheState::Clean);
        assert!(!entry.is_dirty());
        assert!(entry.is_evictable());
    }

    #[test]
    fn mark_dirty_transitions_state() {
        let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        entry.mark_dirty(0..50);
        assert_eq!(entry.state, CacheState::Dirty);
        assert!(entry.is_dirty());
        assert!(!entry.is_evictable());
    }

    #[test]
    fn mark_committing_transitions_state() {
        let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        entry.mark_dirty(0..50);
        entry.mark_committing();
        assert_eq!(entry.state, CacheState::Committing);
        assert!(entry.is_dirty());
        assert!(!entry.is_evictable());
    }

    #[test]
    fn mark_conflicted_transitions_state() {
        let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        entry.mark_dirty(0..50);
        entry.mark_conflicted();
        assert_eq!(entry.state, CacheState::Conflicted);
        assert!(entry.is_dirty());
        assert!(!entry.is_evictable());
    }

    #[test]
    fn mark_clean_resets_state() {
        let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        entry.mark_dirty(0..50);
        entry.mark_clean();
        assert_eq!(entry.state, CacheState::Clean);
        assert!(!entry.is_dirty());
        assert!(entry.is_evictable());
        assert!(entry.dirty_extents.ranges().is_empty());
    }

    #[test]
    fn touch_updates_last_access() {
        let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
        let old_access = entry.last_access;
        std::thread::sleep(std::time::Duration::from_millis(10));
        entry.touch();
        assert!(entry.last_access > old_access);
    }
}
