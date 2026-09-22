//! JSON contract tests for protocol DTOs.

use discordfs_core::NodeKind;
use discordfs_protocol::{
    CommitVersionRequest, CreateNodeRequest, NameBytes, NodeResponse, StageVersionRequest,
};
use uuid::Uuid;

#[test]
fn name_bytes_roundtrip_non_utf8() {
    // Non-UTF8 bytes (Latin-1 encoded "café")
    let bytes = vec![0x63, 0x61, 0x66, 0xe9];
    let name = NameBytes::new(bytes.clone()).unwrap();

    let json = serde_json::to_string(&name).unwrap();
    let decoded: NameBytes = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.as_bytes(), bytes.as_slice());
}

#[test]
fn name_bytes_base64_encoding() {
    let bytes = b"hello.txt".to_vec();
    let name = NameBytes::new(bytes.clone()).unwrap();

    let encoded = name.to_base64();
    let decoded = NameBytes::from_base64(&encoded).unwrap();

    assert_eq!(decoded.as_bytes(), bytes.as_slice());
}

#[test]
fn create_node_request_json() {
    let req = CreateNodeRequest {
        parent_id: Uuid::new_v4(),
        name: NameBytes::new(b"test.txt".to_vec()).unwrap(),
        kind: NodeKind::File,
        mode: 0o644,
        uid: 1000,
        gid: 1000,
        link_target: None,
        idempotency_key: Uuid::new_v4(),
    };

    let json = serde_json::to_string(&req).unwrap();
    let decoded: CreateNodeRequest = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.parent_id, req.parent_id);
    assert_eq!(decoded.name.as_bytes(), req.name.as_bytes());
    assert_eq!(decoded.kind, req.kind);
    assert_eq!(decoded.mode, req.mode);
}

#[test]
fn stage_version_request_json() {
    let req = StageVersionRequest {
        node_id: Uuid::new_v4(),
        expected_generation: 5,
        expected_current_version: Some(Uuid::new_v4()),
        idempotency_key: Uuid::new_v4(),
    };

    let json = serde_json::to_string(&req).unwrap();
    let decoded: StageVersionRequest = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.node_id, req.node_id);
    assert_eq!(decoded.expected_generation, req.expected_generation);
}

#[test]
fn commit_version_request_json() {
    let req = CommitVersionRequest {
        version_id: Uuid::new_v4(),
        total_size: 1024 * 1024,
        plaintext_hash: "abc123".to_string(),
        expected_generation: 5,
        expected_current_version: Some(Uuid::new_v4()),
        idempotency_key: Uuid::new_v4(),
    };

    let json = serde_json::to_string(&req).unwrap();
    let decoded: CommitVersionRequest = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.version_id, req.version_id);
    assert_eq!(decoded.total_size, req.total_size);
    assert_eq!(decoded.plaintext_hash, req.plaintext_hash);
}

#[test]
fn node_response_serialization() {
    use chrono::Utc;

    let resp = NodeResponse {
        id: Uuid::new_v4(),
        parent_id: Some(Uuid::new_v4()),
        name: NameBytes::new(b"test.txt".to_vec()).unwrap(),
        kind: NodeKind::File,
        mode: 0o100644,
        uid: 1000,
        gid: 1000,
        size: 1024,
        atime: Utc::now(),
        mtime: Utc::now(),
        ctime: Utc::now(),
        current_version_id: Some(Uuid::new_v4()),
        generation: 1,
        link_target: None,
    };

    let json = serde_json::to_string(&resp).unwrap();
    let decoded: NodeResponse = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.id, resp.id);
    assert_eq!(decoded.size, resp.size);
    assert_eq!(decoded.generation, resp.generation);
}
