//! Filesystem semantics, driven through the service layer with a fake server.
//!
//! These run on any platform: no kernel, no mount, no `fuser`.

use dcfs_core::NodeKind;
use dcfs_fuse::fake_client::FakeClient;
use dcfs_fuse::{ClientError, Fs, ROOT_INO};
use std::sync::Arc;

async fn mounted() -> Fs<FakeClient> {
    Fs::mount(Arc::new(FakeClient::new()))
        .await
        .expect("fake server always has a root")
}

#[tokio::test]
async fn the_mount_point_is_inode_one() {
    let fs = mounted().await;

    let root = fs.getattr(ROOT_INO).await.unwrap();
    assert_eq!(root.ino, ROOT_INO);
    assert_eq!(root.kind, NodeKind::Directory);
}

#[tokio::test]
async fn an_inode_we_never_handed_out_is_enoent() {
    let fs = mounted().await;
    assert!(matches!(
        fs.getattr(9999).await.unwrap_err(),
        ClientError::NotFound
    ));
}

#[tokio::test]
async fn inodes_are_stable_across_operations() {
    let fs = mounted().await;
    let created = fs
        .create(ROOT_INO, b"stable.txt", NodeKind::File, 0o644, 1000, 1000)
        .await
        .unwrap();

    // The same node must keep the same inode however we reach it, or the
    // kernel's cache would point at the wrong file.
    let looked_up = fs.lookup(ROOT_INO, b"stable.txt").await.unwrap();
    let by_attr = fs.getattr(created.ino).await.unwrap();
    assert_eq!(created.ino, looked_up.ino);
    assert_eq!(created.ino, by_attr.ino);
    assert_ne!(created.ino, ROOT_INO);
}

#[tokio::test]
async fn lookup_finds_children_and_misses_are_enoent() {
    let fs = mounted().await;
    fs.create(ROOT_INO, b"here", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    assert_eq!(fs.lookup(ROOT_INO, b"here").await.unwrap().size, 0);
    assert!(matches!(
        fs.lookup(ROOT_INO, b"missing").await.unwrap_err(),
        ClientError::NotFound
    ));
}

#[tokio::test]
async fn readdir_starts_with_dot_and_dotdot() {
    let fs = mounted().await;
    let dir = fs
        .create(ROOT_INO, b"sub", NodeKind::Directory, 0o755, 0, 0)
        .await
        .unwrap();
    fs.create(dir.ino, b"inner.txt", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    let entries = fs.readdir(dir.ino).await.unwrap();
    assert_eq!(entries[0].name, b".");
    assert_eq!(entries[0].ino, dir.ino);
    assert_eq!(entries[1].name, b"..");
    assert_eq!(entries[1].ino, ROOT_INO, "..' must point at the parent");
    assert_eq!(entries[2].name, b"inner.txt");
    assert_eq!(entries[2].kind, NodeKind::File);

    // The root is its own parent, as the kernel expects at a mount point.
    let root_entries = fs.readdir(ROOT_INO).await.unwrap();
    assert_eq!(root_entries[1].ino, ROOT_INO);
}

#[tokio::test]
async fn readdir_on_a_file_is_rejected() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"notadir", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    assert!(matches!(
        fs.readdir(file.ino).await.unwrap_err(),
        ClientError::InvalidRequest(_)
    ));
}

#[tokio::test]
async fn raw_non_utf8_names_survive_create_lookup_and_readdir() {
    let fs = mounted().await;
    // Linux filenames are bytes, not text.
    let raw = [0xff, 0xfe, b'x'];
    let created = fs
        .create(ROOT_INO, &raw, NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    assert_eq!(fs.lookup(ROOT_INO, &raw).await.unwrap().ino, created.ino);
    let entries = fs.readdir(ROOT_INO).await.unwrap();
    assert!(entries.iter().any(|e| e.name == raw));
}

#[tokio::test]
async fn names_with_a_slash_or_nul_are_rejected_before_they_reach_the_server() {
    let fs = mounted().await;
    for bad in [b"a/b".to_vec(), b"a\0b".to_vec(), Vec::new()] {
        assert!(
            matches!(
                fs.create(ROOT_INO, &bad, NodeKind::File, 0o644, 0, 0)
                    .await
                    .unwrap_err(),
                ClientError::InvalidName
            ),
            "{bad:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn write_then_read_round_trips() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"data.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    assert_eq!(fs.write(file.ino, 0, b"hello world").await.unwrap(), 11);
    assert_eq!(fs.read(file.ino, 0, 11).await.unwrap(), b"hello world");
    assert_eq!(fs.read(file.ino, 6, 5).await.unwrap(), b"world");
    assert_eq!(fs.getattr(file.ino).await.unwrap().size, 11);
}

#[tokio::test]
async fn an_unaligned_overwrite_keeps_the_surrounding_bytes() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"patch.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    fs.write(file.ino, 0, b"aaaaaaaaaa").await.unwrap();
    fs.write(file.ino, 3, b"ZZ").await.unwrap();
    assert_eq!(fs.read(file.ino, 0, 10).await.unwrap(), b"aaaZZaaaaa");
}

#[tokio::test]
async fn rename_moves_a_node_without_touching_its_bytes() {
    let fs = mounted().await;
    let dir = fs
        .create(ROOT_INO, b"dir", NodeKind::Directory, 0o755, 0, 0)
        .await
        .unwrap();
    let file = fs
        .create(ROOT_INO, b"old", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"payload").await.unwrap();

    fs.rename(ROOT_INO, b"old", dir.ino, b"new").await.unwrap();

    assert!(matches!(
        fs.lookup(ROOT_INO, b"old").await.unwrap_err(),
        ClientError::NotFound
    ));
    let moved = fs.lookup(dir.ino, b"new").await.unwrap();
    assert_eq!(moved.ino, file.ino, "renaming must not change the inode");
    assert_eq!(fs.read(file.ino, 0, 7).await.unwrap(), b"payload");
}

#[tokio::test]
async fn remove_deletes_the_entry() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"gone", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    fs.remove(ROOT_INO, b"gone").await.unwrap();
    assert!(matches!(
        fs.lookup(ROOT_INO, b"gone").await.unwrap_err(),
        ClientError::NotFound
    ));
    // The stale inode must not resolve to anything either.
    assert!(matches!(
        fs.getattr(file.ino).await.unwrap_err(),
        ClientError::NotFound
    ));
    assert!(matches!(
        fs.remove(ROOT_INO, b"gone").await.unwrap_err(),
        ClientError::NotFound
    ));
}

#[tokio::test]
async fn setattr_changes_mode_and_size() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"attrs", NodeKind::File, 0o644, 1000, 1000)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"0123456789").await.unwrap();

    let attr = fs
        .setattr(file.ino, Some(0o600), None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(attr.mode, 0o600);
    assert_eq!(attr.size, 10, "mode-only change must not resize");

    // Truncate to zero, the `>` redirect case.
    let attr = fs
        .setattr(file.ino, None, None, None, Some(0), None, None)
        .await
        .unwrap();
    assert_eq!(attr.size, 0);
}

#[tokio::test]
async fn fsync_on_a_known_file_succeeds() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"sync.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"x").await.unwrap();
    fs.fsync(file.ino).await.unwrap();
}

// --- write buffering -------------------------------------------------------

/// Mount with a handle on the same fake server, so tests can see what actually
/// reached it as opposed to what is still buffered.
async fn mounted_with_client() -> (Fs<FakeClient>, Arc<FakeClient>) {
    let client = Arc::new(FakeClient::new());
    let fs = Fs::mount(client.clone()).await.unwrap();
    (fs, client)
}

#[tokio::test]
async fn small_sequential_writes_are_coalesced_until_flushed() {
    use dcfs_fuse::ServerClient;
    let (fs, client) = mounted_with_client().await;
    let file = fs
        .create(ROOT_INO, b"buffered", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    let id = client
        .list_children(FakeClient::root_id(), None, None)
        .await
        .unwrap()
        .children[0]
        .id;

    // Four writes that continue each other stay in the buffer.
    for (index, part) in [b"aaaa", b"bbbb", b"cccc", b"dddd"].iter().enumerate() {
        fs.write(file.ino, index as u64 * 4, *part).await.unwrap();
    }
    assert_eq!(
        client.get_node(id).await.unwrap().size,
        0,
        "nothing should have reached the server yet"
    );

    fs.fsync(file.ino).await.unwrap();
    assert_eq!(client.get_node(id).await.unwrap().size, 16);
    assert_eq!(fs.read(file.ino, 0, 16).await.unwrap(), b"aaaabbbbccccdddd");
}

#[tokio::test]
async fn a_non_contiguous_write_flushes_the_previous_run() {
    let (fs, _client) = mounted_with_client().await;
    let file = fs
        .create(ROOT_INO, b"seeky", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    fs.write(file.ino, 0, b"start").await.unwrap();
    // Seeking past the end breaks the run; both parts must survive.
    fs.write(file.ino, 100, b"end").await.unwrap();
    fs.fsync(file.ino).await.unwrap();

    let content = fs.read(file.ino, 0, 103).await.unwrap();
    assert_eq!(content.len(), 103);
    assert_eq!(&content[..5], b"start");
    assert_eq!(&content[100..], b"end");
}

#[tokio::test]
async fn buffered_bytes_are_visible_to_reads_and_getattr() {
    let (fs, _client) = mounted_with_client().await;
    let file = fs
        .create(ROOT_INO, b"visible", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    fs.write(file.ino, 0, b"unflushed").await.unwrap();

    // Buffering must not be observable through the filesystem itself.
    assert_eq!(fs.getattr(file.ino).await.unwrap().size, 9);
    assert_eq!(fs.read(file.ino, 0, 9).await.unwrap(), b"unflushed");
    assert_eq!(fs.lookup(ROOT_INO, b"visible").await.unwrap().size, 9);

    let entries = fs.readdir(ROOT_INO).await.unwrap();
    assert!(entries.iter().any(|e| e.name == b"visible"));
}

#[tokio::test]
async fn removing_a_file_drops_its_buffered_write() {
    let (fs, _client) = mounted_with_client().await;
    let file = fs
        .create(ROOT_INO, b"doomed", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"never lands").await.unwrap();

    // The flush_all inside lookup runs first, so this also proves the write is
    // not replayed against a node that no longer exists.
    fs.remove(ROOT_INO, b"doomed").await.unwrap();
    assert!(matches!(
        fs.getattr(file.ino).await.unwrap_err(),
        ClientError::NotFound
    ));
}

// --- stream and mirror modes ----------------------------------------------

use dcfs_fuse::{BlockCache, Mode};

/// Mount with a content cache in a throwaway directory.
async fn mounted_with_cache(mode: Mode) -> (Fs<FakeClient>, Arc<BlockCache>) {
    let dir = std::env::temp_dir().join(format!("dfs-svc-{}", uuid::Uuid::new_v4().simple()));
    let cache = Arc::new(BlockCache::open(dir, mode).await.unwrap());
    let fs = Fs::mount_with_cache(Arc::new(FakeClient::new()), Some(cache.clone()))
        .await
        .unwrap();
    (fs, cache)
}

#[tokio::test]
async fn a_read_populates_the_cache_and_returns_the_same_bytes() {
    let (fs, cache) = mounted_with_cache(Mode::Mirror).await;
    let file = fs
        .create(ROOT_INO, b"cached.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"hello cache").await.unwrap();
    fs.fsync(file.ino).await.unwrap();

    assert_eq!(
        cache.block_count(),
        0,
        "nothing cached before the first read"
    );

    // getattr refreshes the version the cache keys on.
    fs.getattr(file.ino).await.unwrap();
    assert_eq!(fs.read(file.ino, 0, 11).await.unwrap(), b"hello cache");
    assert_eq!(cache.block_count(), 1);

    // A second read must still be correct, now served locally.
    assert_eq!(fs.read(file.ino, 6, 5).await.unwrap(), b"cache");

    cache.discard().await;
}

#[tokio::test]
async fn writing_a_file_drops_its_cached_blocks() {
    let (fs, cache) = mounted_with_cache(Mode::Mirror).await;
    let file = fs
        .create(ROOT_INO, b"changing.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"first").await.unwrap();
    fs.fsync(file.ino).await.unwrap();
    fs.getattr(file.ino).await.unwrap();
    fs.read(file.ino, 0, 5).await.unwrap();
    assert_eq!(cache.block_count(), 1);

    fs.write(file.ino, 0, b"SECOND").await.unwrap();
    fs.fsync(file.ino).await.unwrap();
    assert_eq!(
        cache.block_count(),
        0,
        "stale blocks must not survive a write"
    );

    fs.getattr(file.ino).await.unwrap();
    assert_eq!(fs.read(file.ino, 0, 6).await.unwrap(), b"SECOND");

    cache.discard().await;
}

#[tokio::test]
async fn mirror_prefetch_walks_the_whole_tree() {
    let (fs, cache) = mounted_with_cache(Mode::Mirror).await;

    let dir = fs
        .create(ROOT_INO, b"docs", NodeKind::Directory, 0o755, 0, 0)
        .await
        .unwrap();
    let nested = fs
        .create(dir.ino, b"deep", NodeKind::Directory, 0o755, 0, 0)
        .await
        .unwrap();
    for (parent, name) in [
        (ROOT_INO, &b"top.txt"[..]),
        (dir.ino, b"mid.txt"),
        (nested.ino, b"low.txt"),
    ] {
        let file = fs
            .create(parent, name, NodeKind::File, 0o644, 0, 0)
            .await
            .unwrap();
        fs.write(file.ino, 0, b"payload").await.unwrap();
        fs.fsync(file.ino).await.unwrap();
    }

    let (files, bytes) = fs.prefetch_all().await.unwrap();
    assert_eq!(files, 3, "every file in every directory");
    assert_eq!(bytes, 21);
    assert_eq!(cache.block_count(), 3);

    cache.discard().await;
}

#[tokio::test]
async fn an_empty_file_is_readable_and_caches_nothing() {
    let (fs, cache) = mounted_with_cache(Mode::Mirror).await;
    let file = fs
        .create(ROOT_INO, b"empty.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();

    // No committed version means no cache key, and must not error.
    fs.getattr(file.ino).await.unwrap();
    assert!(fs.read(file.ino, 0, 10).await.unwrap().is_empty());
    assert_eq!(cache.block_count(), 0);

    let (files, bytes) = fs.prefetch_all().await.unwrap();
    assert_eq!((files, bytes), (1, 0));

    cache.discard().await;
}

#[tokio::test]
async fn stream_mode_keeps_the_cache_inside_its_budget() {
    // Room for one block only.
    let (fs, cache) = mounted_with_cache(Mode::Stream {
        budget_bytes: dcfs_fuse::BLOCK_SIZE,
    })
    .await;

    for index in 0..4u8 {
        let file = fs
            .create(ROOT_INO, &[b'f', b'0' + index], NodeKind::File, 0o644, 0, 0)
            .await
            .unwrap();
        fs.write(file.ino, 0, b"some bytes").await.unwrap();
        fs.fsync(file.ino).await.unwrap();
        fs.getattr(file.ino).await.unwrap();
        fs.read(file.ino, 0, 10).await.unwrap();
    }

    assert!(
        cache.bytes() <= dcfs_fuse::BLOCK_SIZE,
        "stream mode must stay within its budget, held {} bytes",
        cache.bytes()
    );

    cache.discard().await;
}

#[tokio::test]
async fn rename_over_an_existing_file_replaces_it() {
    let fs = mounted().await;

    // save-to-temp-then-rename, the idiom git and editors rely on.
    let target = fs
        .create(ROOT_INO, b"config", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(target.ino, 0, b"old contents").await.unwrap();
    let temp = fs
        .create(ROOT_INO, b"config.lock", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(temp.ino, 0, b"new contents").await.unwrap();

    fs.rename(ROOT_INO, b"config.lock", ROOT_INO, b"config")
        .await
        .unwrap();

    let now = fs.lookup(ROOT_INO, b"config").await.unwrap();
    assert_eq!(now.ino, temp.ino, "the renamed file kept its identity");
    assert_eq!(fs.read(now.ino, 0, 12).await.unwrap(), b"new contents");
    assert!(matches!(
        fs.lookup(ROOT_INO, b"config.lock").await.unwrap_err(),
        ClientError::NotFound
    ));

    // Exactly one entry is left, not two.
    let entries = fs.readdir(ROOT_INO).await.unwrap();
    assert_eq!(entries.iter().filter(|e| e.name == b"config").count(), 1);
    assert_eq!(entries.len(), 3, ". .. and one file");
}

#[tokio::test]
async fn symlinks_round_trip_their_raw_target() {
    let fs = mounted().await;

    let link = fs
        .symlink(ROOT_INO, b"link", b"../some/target", 1000, 1000)
        .await
        .unwrap();
    assert_eq!(link.kind, NodeKind::Symlink);
    assert_eq!(fs.readlink(link.ino).await.unwrap(), b"../some/target");

    // It shows up as a link in the directory, not as a file.
    let entries = fs.readdir(ROOT_INO).await.unwrap();
    let entry = entries.iter().find(|e| e.name == b"link").unwrap();
    assert_eq!(entry.kind, NodeKind::Symlink);

    // A target need not exist, need not be UTF-8, and is never resolved here.
    let raw = fs
        .symlink(ROOT_INO, b"raw-link", &[0xff, b'/', 0xfe], 0, 0)
        .await
        .unwrap();
    assert_eq!(fs.readlink(raw.ino).await.unwrap(), [0xff, b'/', 0xfe]);
}

#[tokio::test]
async fn an_empty_symlink_target_is_rejected() {
    let fs = mounted().await;
    assert!(fs.symlink(ROOT_INO, b"bad", b"", 0, 0).await.is_err());
}

#[tokio::test]
async fn readlink_on_a_regular_file_is_an_error() {
    let fs = mounted().await;
    let file = fs
        .create(ROOT_INO, b"plain.txt", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    assert!(fs.readlink(file.ino).await.is_err());
}

#[tokio::test]
async fn writes_flush_on_part_boundaries() {
    let (fs, client) = mounted_with_client().await;
    use dcfs_fuse::ServerClient;
    let part = client.fs_info().await.unwrap().chunk_size;

    let file = fs
        .create(ROOT_INO, b"aligned.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    let id = client
        .list_children(FakeClient::root_id(), None, None)
        .await
        .unwrap()
        .children[0]
        .id;

    // Write one part in small pieces. Nothing should reach the server until
    // the run ends exactly on the boundary, and then the whole part goes at
    // once — which is what lets the server skip reading the old part back.
    let piece = (part / 4) as usize;
    for i in 0..3 {
        fs.write(file.ino, (i * piece) as u64, &vec![b'a'; piece])
            .await
            .unwrap();
        assert_eq!(
            client.get_node(id).await.unwrap().size,
            0,
            "still buffering below the boundary"
        );
    }
    fs.write(file.ino, (3 * piece) as u64, &vec![b'a'; piece])
        .await
        .unwrap();
    assert_eq!(
        client.get_node(id).await.unwrap().size,
        part,
        "the run ending on a part boundary is sent immediately"
    );

    // Content is still exactly what was written.
    assert_eq!(
        fs.read(file.ino, 0, part).await.unwrap(),
        vec![b'a'; part as usize]
    );
}

#[tokio::test]
async fn attributes_never_report_less_than_was_accepted() {
    let (fs, client) = mounted_with_client().await;
    use dcfs_fuse::ServerClient;
    let file = fs
        .create(ROOT_INO, b"growing.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    let id = client
        .list_children(FakeClient::root_id(), None, None)
        .await
        .unwrap()
        .children[0]
        .id;

    // Accept bytes that are still buffered: the server has not seen them.
    fs.write(file.ino, 0, b"buffered bytes").await.unwrap();
    assert_eq!(client.get_node(id).await.unwrap().size, 0);

    // The kernel takes an attribute reply as authoritative and caches it, so a
    // size that lags the writes it already acknowledged makes reads stop short.
    let seen_via_lookup = fs.lookup(ROOT_INO, b"growing.bin").await.unwrap();
    assert_eq!(seen_via_lookup.size, 14);
    assert_eq!(fs.getattr(file.ino).await.unwrap().size, 14);

    let entries = fs.readdir(ROOT_INO).await.unwrap();
    assert!(entries.iter().any(|e| e.name == b"growing.bin"));

    // A truncate is authoritative in both directions.
    fs.setattr(file.ino, None, None, None, Some(4), None, None)
        .await
        .unwrap();
    assert_eq!(fs.getattr(file.ino).await.unwrap().size, 4);
}

// --- durable writes --------------------------------------------------------

use dcfs_fuse::WriteLog;

#[tokio::test]
async fn a_buffered_write_survives_the_process_dying() {
    use dcfs_fuse::ServerClient;
    let dir = std::env::temp_dir().join(format!("dfs-wl-{}", uuid::Uuid::new_v4().simple()));
    let client = Arc::new(FakeClient::new());
    let log = Arc::new(WriteLog::open(&dir).unwrap());

    let fs = Fs::mount_with(client.clone(), None, Some(log.clone()))
        .await
        .unwrap();
    let file = fs
        .create(ROOT_INO, b"durable.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    let id = client
        .list_children(FakeClient::root_id(), None, None)
        .await
        .unwrap()
        .children[0]
        .id;

    // Accepted by us, acknowledged to the caller, not yet sent.
    fs.write(file.ino, 0, b"unsent bytes").await.unwrap();
    assert_eq!(client.get_node(id).await.unwrap().size, 0);

    // The process dies here: drop everything but the server and the log.
    drop(fs);

    let recovered = Fs::mount_with(
        client.clone(),
        None,
        Some(Arc::new(WriteLog::open(&dir).unwrap())),
    )
    .await
    .unwrap();
    assert_eq!(recovered.recover().await.unwrap(), 1);
    assert_eq!(client.get_node(id).await.unwrap().size, 12);
    assert_eq!(client.read_file(id, 0, 12).await.unwrap(), b"unsent bytes");

    // Recovery is not repeated on the next mount.
    let again = Fs::mount_with(client, None, Some(Arc::new(WriteLog::open(&dir).unwrap())))
        .await
        .unwrap();
    assert_eq!(again.recover().await.unwrap(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_sent_write_is_not_replayed() {
    let dir = std::env::temp_dir().join(format!("dfs-wl-{}", uuid::Uuid::new_v4().simple()));
    let client = Arc::new(FakeClient::new());
    let log = Arc::new(WriteLog::open(&dir).unwrap());

    let fs = Fs::mount_with(client.clone(), None, Some(log.clone()))
        .await
        .unwrap();
    let file = fs
        .create(ROOT_INO, b"sent.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"sent").await.unwrap();
    fs.fsync(file.ino).await.unwrap();

    // Replaying a write the server already has would be harmless here, but it
    // would not be for a write that has since been overwritten.
    let next = Fs::mount_with(client, None, Some(Arc::new(WriteLog::open(&dir).unwrap())))
        .await
        .unwrap();
    assert_eq!(next.recover().await.unwrap(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn deleting_a_file_drops_its_durable_record_too() {
    let dir = std::env::temp_dir().join(format!("dfs-wl-{}", uuid::Uuid::new_v4().simple()));
    let client = Arc::new(FakeClient::new());
    let fs = Fs::mount_with(
        client.clone(),
        None,
        Some(Arc::new(WriteLog::open(&dir).unwrap())),
    )
    .await
    .unwrap();

    let file = fs
        .create(ROOT_INO, b"doomed.bin", NodeKind::File, 0o644, 0, 0)
        .await
        .unwrap();
    fs.write(file.ino, 0, b"never lands").await.unwrap();
    fs.remove(ROOT_INO, b"doomed.bin").await.unwrap();

    // Replaying a write to a file that no longer exists would only log errors.
    let next = Fs::mount_with(client, None, Some(Arc::new(WriteLog::open(&dir).unwrap())))
        .await
        .unwrap();
    assert_eq!(next.recover().await.unwrap(), 0);

    std::fs::remove_dir_all(&dir).ok();
}
