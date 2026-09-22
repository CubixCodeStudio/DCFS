//! Cache integration tests.

// Comparing `ranges()` against a slice that happens to hold one range is
// exactly what these assertions mean.
#![allow(clippy::single_range_in_vec_init)]

use discordfs_cache::{CacheManager, DirtyExtents, Journal, JournalEntry};
use std::path::PathBuf;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn dirty_extents_merge() {
    let mut extents = DirtyExtents::new();
    extents.insert(0..100);
    extents.insert(50..150);
    assert_eq!(extents.ranges(), &[0..150]);
}

#[test]
fn dirty_entries_survive_eviction() {
    let dir = TempDir::new().unwrap();
    let mut mgr = CacheManager::new(dir.path().to_path_buf(), 50);
    let node_id = Uuid::new_v4();

    let entry = mgr.get_or_create(node_id, None, 0, 100);
    entry.mark_dirty(0..100);

    mgr.evict_if_needed();

    assert!(mgr.get(&node_id).is_some());
}

#[test]
fn clean_entries_evicted_under_pressure() {
    let dir = TempDir::new().unwrap();
    let mut mgr = CacheManager::new(dir.path().to_path_buf(), 50);
    let node_id = Uuid::new_v4();

    mgr.get_or_create(node_id, None, 0, 100);
    mgr.evict_if_needed();

    assert!(mgr.get(&node_id).is_none());
}

#[test]
fn journal_recovery() {
    let dir = TempDir::new().unwrap();
    let journal = Journal::new(dir.path());

    let entry = JournalEntry {
        node_id: Uuid::new_v4(),
        base_version_id: None,
        generation: 1,
        local_path: PathBuf::from("/tmp/test"),
        offset: 0,
        logical_size: 1024,
        dirty: true,
    };

    journal.record_dirty(&entry).unwrap();
    let recovered = journal.recover().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].node_id, entry.node_id);
}

#[test]
fn journal_multiple_entries() {
    let dir = TempDir::new().unwrap();
    let journal = Journal::new(dir.path());

    let entry1 = JournalEntry {
        node_id: Uuid::new_v4(),
        base_version_id: None,
        generation: 1,
        local_path: PathBuf::from("/tmp/test1"),
        offset: 0,
        logical_size: 1024,
        dirty: true,
    };

    let entry2 = JournalEntry {
        node_id: Uuid::new_v4(),
        base_version_id: Some(Uuid::new_v4()),
        generation: 2,
        local_path: PathBuf::from("/tmp/test2"),
        offset: 0,
        logical_size: 2048,
        dirty: true,
    };

    journal.record_dirty(&entry1).unwrap();
    journal.record_dirty(&entry2).unwrap();
    let recovered = journal.recover().unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(recovered[0].node_id, entry1.node_id);
    assert_eq!(recovered[1].node_id, entry2.node_id);
}

#[test]
fn cache_entry_state_transitions() {
    use discordfs_cache::{CacheEntry, CacheState};

    let mut entry = CacheEntry::new(Uuid::new_v4(), None, 0, PathBuf::from("/tmp/test"), 100);
    assert_eq!(entry.state, CacheState::Clean);
    assert!(entry.is_evictable());

    entry.mark_dirty(0..50);
    assert_eq!(entry.state, CacheState::Dirty);
    assert!(!entry.is_evictable());

    entry.mark_committing();
    assert_eq!(entry.state, CacheState::Committing);
    assert!(!entry.is_evictable());

    entry.mark_clean();
    assert_eq!(entry.state, CacheState::Clean);
    assert!(entry.is_evictable());
}

#[test]
fn dirty_extents_complex_merge() {
    let mut extents = DirtyExtents::new();
    extents.insert(0..10);
    extents.insert(20..30);
    extents.insert(40..50);
    extents.insert(5..45); // Should merge all three
    assert_eq!(extents.ranges(), &[0..50]);
}

#[test]
fn manager_preserves_dirty_on_eviction() {
    let dir = TempDir::new().unwrap();
    let mut mgr = CacheManager::new(dir.path().to_path_buf(), 50);

    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();

    let entry1 = mgr.get_or_create(node1, None, 0, 100);
    entry1.mark_dirty(0..100);

    let entry2 = mgr.get_or_create(node2, None, 0, 100);
    entry2.mark_dirty(0..100);

    mgr.evict_if_needed();

    // Both dirty entries should still exist
    assert!(mgr.get(&node1).is_some());
    assert!(mgr.get(&node2).is_some());
}
