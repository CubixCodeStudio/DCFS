//! Domain model integration tests.

use discordfs_core::*;

#[test]
fn rejects_slash_and_nul_names() {
    assert!(NodeName::new(b"a/b".to_vec()).is_err());
    assert!(NodeName::new(b"a\0b".to_vec()).is_err());
}

#[test]
fn plans_unaligned_last_chunk() {
    let chunks = plan_chunks(17, 8).unwrap();
    assert_eq!(
        chunks.iter().map(|c| c.len).collect::<Vec<_>>(),
        vec![8, 8, 1]
    );
}

#[test]
fn version_state_transitions() {
    // Staging -> Committed -> Superseded -> Garbage
    let state = VersionState::Staging;
    let state = state.transition_to(VersionState::Committed).unwrap();
    let state = state.transition_to(VersionState::Superseded).unwrap();
    let state = state.transition_to(VersionState::Garbage).unwrap();
    assert!(state.is_terminal());
}

#[test]
fn node_kinds_are_distinct() {
    let dir = Node::new_directory(
        NodeId::new(),
        None,
        NodeName::new(b"dir".to_vec()).unwrap(),
        FileAttr::default(),
    );
    let file = Node::new_file(
        NodeId::new(),
        None,
        NodeName::new(b"file".to_vec()).unwrap(),
        FileAttr::default(),
    );
    assert!(dir.is_directory());
    assert!(!dir.is_file());
    assert!(file.is_file());
    assert!(!file.is_directory());
}

#[test]
fn ids_are_unique() {
    let ids: Vec<_> = (0..100).map(|_| NodeId::new()).collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len());
}

#[test]
fn chunk_planning_handles_boundaries() {
    // Exact boundary
    let chunks = plan_chunks(1024, 256).unwrap();
    assert_eq!(chunks.len(), 4);

    // One byte over
    let chunks = plan_chunks(1025, 256).unwrap();
    assert_eq!(chunks.len(), 5);
    assert_eq!(chunks[4].len, 1);

    // One byte under
    let chunks = plan_chunks(1023, 256).unwrap();
    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[3].len, 255);
}
