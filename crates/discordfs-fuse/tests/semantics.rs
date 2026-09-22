//! Semantics tests for DiscordFS operations using the fake client.

#[cfg(test)]
mod tests {
    use discordfs_core::NodeKind;
    use discordfs_fuse::client::ServerClient;
    use discordfs_fuse::fake_client::FakeClient;
    use discordfs_protocol::{CreateNodeRequest, NameBytes, RenameNodeRequest};
    use uuid::Uuid;

    #[tokio::test]
    async fn test_create_and_list_files() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"test.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();

        // List children
        let children = client.list_children(root_id, None, None).await.unwrap();
        assert_eq!(children.children.len(), 1);
        assert_eq!(children.children[0].id, file.id);
        assert_eq!(children.children[0].name.as_bytes(), b"test.txt");
    }

    #[tokio::test]
    async fn test_create_directory() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a directory
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"subdir".to_vec()).unwrap(),
            kind: NodeKind::Directory,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let dir = client.create_node(req).await.unwrap();

        assert_eq!(dir.kind, NodeKind::Directory);
        assert_eq!(dir.name.as_bytes(), b"subdir");
    }

    #[tokio::test]
    async fn test_write_and_read_file() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"data.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();

        // Write data
        let data = b"Hello, World!";
        let written = client.write_file(file.id, 0, data).await.unwrap();
        assert_eq!(written, data.len() as u64);

        // Read data back
        let read_data = client
            .read_file(file.id, 0, data.len() as u64)
            .await
            .unwrap();
        assert_eq!(read_data, data);

        // Verify size updated
        let node = client.get_node(file.id).await.unwrap();
        assert_eq!(node.size, data.len() as u64);
    }

    #[tokio::test]
    async fn test_read_partial_file() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create and write to file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"partial.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();
        client.write_file(file.id, 0, b"0123456789").await.unwrap();

        // Read middle portion
        let data = client.read_file(file.id, 3, 4).await.unwrap();
        assert_eq!(data, b"3456");
    }

    #[tokio::test]
    async fn test_rename_file() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"old.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();

        // Rename it
        let rename_req = RenameNodeRequest {
            new_parent_id: root_id,
            new_name: NameBytes::new(b"new.txt".to_vec()).unwrap(),
            idempotency_key: Uuid::new_v4(),
        };
        let renamed = client.rename_node(file.id, rename_req).await.unwrap();

        assert_eq!(renamed.name.as_bytes(), b"new.txt");
        assert_eq!(renamed.parent_id, Some(root_id));
    }

    #[tokio::test]
    async fn test_delete_file() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"delete_me.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();

        // Delete it
        client.delete_node(file.id).await.unwrap();

        // Verify it's gone
        let children = client.list_children(root_id, None, None).await.unwrap();
        assert_eq!(children.children.len(), 0);
    }

    #[tokio::test]
    async fn test_cannot_delete_nonempty_directory() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a directory
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"dir".to_vec()).unwrap(),
            kind: NodeKind::Directory,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let dir = client.create_node(req).await.unwrap();

        // Add a file to it
        let file_req = CreateNodeRequest {
            parent_id: dir.id,
            name: NameBytes::new(b"file.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        client.create_node(file_req).await.unwrap();

        // Try to delete directory - should fail
        let result = client.delete_node(dir.id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_duplicate_name_rejected() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create first file
        let req1 = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"dup.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        client.create_node(req1).await.unwrap();

        // Try to create another with same name
        let req2 = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"dup.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let result = client.create_node(req2).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_nonexistent_node() {
        let client = FakeClient::new();
        let fake_id = Uuid::new_v4();

        let result = client.get_node(fake_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_at_offset() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create a file
        let req = CreateNodeRequest {
            parent_id: root_id,
            name: NameBytes::new(b"offset.txt".to_vec()).unwrap(),
            kind: NodeKind::File,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            link_target: None,
            idempotency_key: Uuid::new_v4(),
        };
        let file = client.create_node(req).await.unwrap();

        // Write at offset 5
        let written = client.write_file(file.id, 5, b"world").await.unwrap();
        assert_eq!(written, 5);

        // Read back - should have zeros at start
        let data = client.read_file(file.id, 0, 10).await.unwrap();
        assert_eq!(data, b"\0\0\0\0\0world");
    }

    #[tokio::test]
    async fn test_multiple_children_sorted() {
        let client = FakeClient::new();
        let root_id = FakeClient::root_id();

        // Create files in non-alphabetical order
        for name in &[b"zebra.txt", b"apple.txt", b"mango.txt"] {
            let req = CreateNodeRequest {
                parent_id: root_id,
                name: NameBytes::new(name.to_vec()).unwrap(),
                kind: NodeKind::File,
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                link_target: None,
                idempotency_key: Uuid::new_v4(),
            };
            client.create_node(req).await.unwrap();
        }

        // List should be sorted
        let children = client.list_children(root_id, None, None).await.unwrap();
        assert_eq!(children.children.len(), 3);
        assert_eq!(children.children[0].name.as_bytes(), b"apple.txt");
        assert_eq!(children.children[1].name.as_bytes(), b"mango.txt");
        assert_eq!(children.children[2].name.as_bytes(), b"zebra.txt");
    }
}
