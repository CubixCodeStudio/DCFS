//! Filesystem logic, independent of any kernel interface.
//!
//! The kernel talks in inode numbers, the server in UUIDs, so this layer owns
//! the mapping between them and every operation the FUSE adapter needs. It has
//! no `fuser` dependency, which is what lets it be tested on any platform.

use crate::blockcache::{BlockCache, BLOCK_SIZE};
use crate::client::{ClientError, ServerClient};
use crate::writelog::WriteLog;
use chrono::{DateTime, Utc};
use discordfs_core::NodeKind;
use discordfs_protocol::{
    CreateNodeRequest, NameBytes, NodeResponse, PatchNodeRequest, RenameNodeRequest,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// FUSE always addresses the mount point as inode 1.
pub const ROOT_INO: u64 = 1;

/// How many children one `list_children` page asks for.
const PAGE: u32 = 1000;

/// Capacity reported by `statfs`: 1 PiB, large enough not to stop anything.
const STATFS_NOMINAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

/// Cap on a buffered run when its end never reaches a part boundary, and the
/// fallback when the server's part size is unknown.
///
/// The kernel hands FUSE small writes — 4 KiB at a time from `cp` — and each
/// one that reaches the server costs a metadata commit, so runs are coalesced.
/// Where the flush lands matters as much as its size: a run ending on a part
/// boundary lets the server replace whole parts instead of downloading,
/// decrypting and re-encrypting them to merge a few bytes in.
const WRITE_BUFFER_BYTES: u64 = 4 * 1024 * 1024;

/// Node attributes in kernel terms, with no `fuser` types involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub kind: NodeKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
    pub ctime: DateTime<Utc>,
}

/// What `statfs` reports. Plain numbers, no `fuser` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub name_max: u32,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub kind: NodeKind,
    /// Raw filename bytes, exactly as Linux stores them.
    pub name: Vec<u8>,
}

/// Inode numbers are assigned by this process and never leave it, so they can
/// be small and dense even though node ids are UUIDs.
#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, Uuid>,
    by_id: HashMap<Uuid, u64>,
    /// The committed version and size last seen for an inode, remembered so a
    /// cached read does not need a metadata round trip of its own. The kernel
    /// calls lookup or getattr before it reads, which is what keeps this fresh.
    seen: HashMap<u64, (Option<Uuid>, u64)>,
    /// The end of the furthest write this process has accepted from the kernel
    /// for an inode, whether or not it has reached the server yet.
    ///
    /// Writes are buffered, so the server's size lags what the kernel has
    /// already been told is written. Reporting that smaller size back shrinks
    /// the kernel's idea of the file, and it trusts that for the whole
    /// attribute TTL: a file copied in one go then reads back truncated, with
    /// no error anywhere. Attributes are reported as the larger of the two.
    high_water: HashMap<u64, u64>,
    next: u64,
}

/// One pending run of contiguous bytes for a file.
struct PendingWrite {
    offset: u64,
    data: Vec<u8>,
    /// Where the same bytes are recorded on disk, so the run survives this
    /// process dying before it reaches the server.
    log_path: Option<PathBuf>,
}

pub struct Fs<C: ServerClient> {
    client: Arc<C>,
    /// The server's part size, so writes can be flushed on its boundaries.
    chunk_size: u64,
    /// Durable record of accepted-but-unsent writes. `None` keeps them in
    /// memory only, which is what the tests that do not care use.
    log: Option<Arc<WriteLog>>,
    cache: Option<Arc<BlockCache>>,
    inodes: Mutex<Inodes>,
    /// Buffered writes, keyed by inode. An async lock, because flushing sends
    /// a request, and holding it also serializes the kernel's parallel writes
    /// into one contiguous run.
    pending: tokio::sync::Mutex<HashMap<u64, PendingWrite>>,
}

impl<C: ServerClient> Fs<C> {
    /// Resolve the server's root and bind it to inode 1, with no local cache:
    /// every read goes to the server.
    pub async fn mount(client: Arc<C>) -> Result<Self, ClientError> {
        Self::mount_with_cache(client, None).await
    }

    /// Resolve the root and bind it to inode 1, serving reads through `cache`.
    pub async fn mount_with_cache(
        client: Arc<C>,
        cache: Option<Arc<BlockCache>>,
    ) -> Result<Self, ClientError> {
        Self::mount_with(client, cache, None).await
    }

    /// Resolve the root, serve reads through `cache`, and make buffered writes
    /// durable through `log`.
    pub async fn mount_with(
        client: Arc<C>,
        cache: Option<Arc<BlockCache>>,
        log: Option<Arc<WriteLog>>,
    ) -> Result<Self, ClientError> {
        let root = client.get_node_by_path("/").await?;
        // Learn the part size once, at mount: it decides where writes flush.
        let chunk_size = client.fs_info().await?.chunk_size.max(1);
        let mut inodes = Inodes {
            next: ROOT_INO + 1,
            ..Default::default()
        };
        inodes.by_ino.insert(ROOT_INO, root.id);
        inodes.by_id.insert(root.id, ROOT_INO);
        Ok(Self {
            client,
            chunk_size,
            log,
            cache,
            inodes: Mutex::new(inodes),
            pending: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The inode for a node id, allocating one the first time we see it.
    fn ino_for(&self, id: Uuid) -> u64 {
        let mut inodes = self.inodes.lock();
        if let Some(&ino) = inodes.by_id.get(&id) {
            return ino;
        }
        let ino = inodes.next;
        inodes.next += 1;
        inodes.by_ino.insert(ino, id);
        inodes.by_id.insert(id, ino);
        ino
    }

    /// The node id behind an inode. An inode we never handed out is ENOENT.
    fn id_for(&self, ino: u64) -> Result<Uuid, ClientError> {
        self.inodes
            .lock()
            .by_ino
            .get(&ino)
            .copied()
            .ok_or(ClientError::NotFound)
    }

    fn attr_of(&self, node: &NodeResponse) -> Attr {
        let ino = self.ino_for(node.id);
        let size = {
            let mut inodes = self.inodes.lock();
            inodes
                .seen
                .insert(ino, (node.current_version_id, node.size));
            node.size
                .max(inodes.high_water.get(&ino).copied().unwrap_or(0))
        };
        Attr {
            ino,
            size,
            kind: node.kind,
            mode: node.mode,
            uid: node.uid,
            gid: node.gid,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
        }
    }

    /// Every child of a directory, following pagination.
    async fn children_of(&self, parent_id: Uuid) -> Result<Vec<NodeResponse>, ClientError> {
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .client
                .list_children(parent_id, Some(PAGE), Some(offset))
                .await?;
            let count = page.children.len();
            all.extend(page.children);
            if count < PAGE as usize {
                return Ok(all);
            }
            offset += PAGE;
        }
    }

    /// Send one file's buffered bytes, if it has any.
    ///
    /// Anything that can observe a file's contents or size calls this first,
    /// so buffering is invisible: only the request count changes.
    pub async fn flush(&self, ino: u64) -> Result<(), ClientError> {
        // The lock is held across the request, not just across the removal.
        // Releasing it first lets a concurrent write buffer and send the *next*
        // run while this one is still in flight, and the two land out of order:
        // the file ends up holding the earlier bytes on top of the later ones.
        let mut pending = self.pending.lock().await;
        if let Some(buffered) = pending.remove(&ino) {
            let node_id = self.id_for(ino)?;
            self.send(node_id, &buffered).await?;
            if let Some(cache) = self.cache.as_ref() {
                cache.invalidate(node_id).await;
            }
        }
        Ok(())
    }

    /// Begin recording a run, on disk as well as in memory.
    fn start_run(&self, node_id: Uuid, offset: u64, data: &[u8]) -> PendingWrite {
        let log_path = self.log.as_ref().and_then(|log| {
            log.begin(node_id, offset, data)
                .map_err(|e| tracing::warn!("cannot record a write: {e}"))
                .ok()
        });
        PendingWrite {
            offset,
            data: data.to_vec(),
            log_path,
        }
    }

    /// Send a run and drop its durable record, in that order: a record that
    /// outlives the send is replayed harmlessly, one dropped early is lost.
    async fn send(&self, node_id: Uuid, run: &PendingWrite) -> Result<(), ClientError> {
        self.client
            .write_file(node_id, run.offset, &run.data)
            .await?;
        if let (Some(log), Some(path)) = (self.log.as_ref(), run.log_path.as_ref()) {
            log.finish(path);
        }
        Ok(())
    }

    /// Replay writes a previous run accepted but never sent.
    ///
    /// Runs are replayed in the order they were recorded, which is the order
    /// they were accepted, so overlapping ones land the same way round.
    pub async fn recover(&self) -> Result<usize, ClientError> {
        let Some(log) = self.log.as_ref() else {
            return Ok(0);
        };
        let pending = log
            .take_pending()
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        if pending.is_empty() {
            return Ok(0);
        }

        tracing::info!(runs = pending.len(), "replaying writes from a previous run");
        let mut replayed = 0;
        for run in &pending {
            match self
                .client
                .write_file(run.node_id, run.offset, &run.data)
                .await
            {
                Ok(_) => replayed += 1,
                // A file that no longer exists cannot be written back, and one
                // unreplayable run must not block the rest.
                Err(e) => tracing::warn!(
                    node = %run.node_id,
                    offset = run.offset,
                    "cannot replay a write: {e}"
                ),
            }
        }
        log.reset()
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        Ok(replayed)
    }

    /// Send every file's buffered bytes, for operations that can observe more
    /// than one file, such as a directory listing.
    async fn flush_all(&self) -> Result<(), ClientError> {
        // Held across the requests for the same reason as `flush`.
        let mut pending = self.pending.lock().await;
        let drained: Vec<(u64, PendingWrite)> = pending.drain().collect();
        for (ino, buffered) in drained {
            let node_id = self.id_for(ino)?;
            self.send(node_id, &buffered).await?;
            if let Some(cache) = self.cache.as_ref() {
                cache.invalidate(node_id).await;
            }
        }
        Ok(())
    }

    /// Filesystem-wide totals for `statfs`.
    ///
    /// The backing store has no fixed capacity, so this reports a large, fixed
    /// figure rather than a real one: tools that check for free space before
    /// writing need an answer, and refusing the call makes `df` fail outright.
    pub fn statfs(&self) -> FsStats {
        FsStats {
            block_size: self.chunk_size.min(u32::MAX as u64) as u32,
            // ponytail: a nominal capacity. A real figure needs the backend to
            // report one, which neither a directory nor Discord does usefully.
            total_blocks: STATFS_NOMINAL_BYTES / self.chunk_size.max(1),
            free_blocks: STATFS_NOMINAL_BYTES / self.chunk_size.max(1),
            name_max: 255,
        }
    }

    pub async fn getattr(&self, ino: u64) -> Result<Attr, ClientError> {
        // The size the caller is about to see must include buffered bytes.
        self.flush(ino).await?;
        let node = self.client.get_node(self.id_for(ino)?).await?;
        Ok(self.attr_of(&node))
    }

    /// Resolve one path component inside a directory.
    pub async fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Attr, ClientError> {
        self.flush_all().await?;
        let parent_id = self.id_for(parent_ino)?;
        // One indexed lookup, not a listing: the cost of opening a file must
        // not grow with the size of the directory holding it.
        let child = self.client.find_child(parent_id, name).await?;
        Ok(self.attr_of(&child))
    }

    /// `.` and `..` first, then the real entries.
    pub async fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>, ClientError> {
        self.flush_all().await?;
        let id = self.id_for(ino)?;
        let node = self.client.get_node(id).await?;
        if node.kind != NodeKind::Directory {
            return Err(ClientError::InvalidRequest("not a directory".into()));
        }
        // The root's parent is itself, which is what the kernel expects.
        let parent_ino = node.parent_id.map(|p| self.ino_for(p)).unwrap_or(ino);

        let mut entries = vec![
            DirEntry {
                ino,
                kind: NodeKind::Directory,
                name: b".".to_vec(),
            },
            DirEntry {
                ino: parent_ino,
                kind: NodeKind::Directory,
                name: b"..".to_vec(),
            },
        ];
        for child in self.children_of(id).await? {
            entries.push(DirEntry {
                ino: self.ino_for(child.id),
                kind: child.kind,
                name: child.name.as_bytes().to_vec(),
            });
        }
        Ok(entries)
    }

    /// Create a file or a directory. Raw name bytes in, validated server-side too.
    pub async fn create(
        &self,
        parent_ino: u64,
        name: &[u8],
        kind: NodeKind,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Attr, ClientError> {
        let parent_id = self.id_for(parent_ino)?;
        let name = NameBytes::new(name.to_vec()).map_err(|_| ClientError::InvalidName)?;
        let node = self
            .client
            .create_node(CreateNodeRequest {
                parent_id,
                name,
                kind,
                mode,
                uid,
                gid,
                link_target: None,
                idempotency_key: Uuid::new_v4(),
            })
            .await?;
        Ok(self.attr_of(&node))
    }

    /// Create a symbolic link pointing at `target`.
    pub async fn symlink(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Attr, ClientError> {
        let parent_id = self.id_for(parent_ino)?;
        let name = NameBytes::new(name.to_vec()).map_err(|_| ClientError::InvalidName)?;
        if target.is_empty() {
            return Err(ClientError::InvalidRequest("empty symlink target".into()));
        }
        let node = self
            .client
            .create_node(CreateNodeRequest {
                parent_id,
                name,
                kind: NodeKind::Symlink,
                // A symlink's own mode is fixed; the target's permissions are
                // what the kernel actually checks.
                mode: 0o120777,
                uid,
                gid,
                link_target: Some(NameBytes(target.to_vec())),
                idempotency_key: Uuid::new_v4(),
            })
            .await?;
        Ok(self.attr_of(&node))
    }

    /// The raw target bytes of a symlink.
    pub async fn readlink(&self, ino: u64) -> Result<Vec<u8>, ClientError> {
        let node = self.client.get_node(self.id_for(ino)?).await?;
        match node.link_target {
            Some(target) => Ok(target.as_bytes().to_vec()),
            None => Err(ClientError::InvalidRequest("not a symlink".into())),
        }
    }

    pub async fn read(&self, ino: u64, offset: u64, size: u64) -> Result<Vec<u8>, ClientError> {
        // Read-after-write must see the bytes, so send them first.
        self.flush(ino).await?;
        let node_id = self.id_for(ino)?;

        let Some(cache) = self.cache.as_ref() else {
            return self.client.read_file(node_id, offset, size).await;
        };

        // Blocks are keyed by version, so a file with nothing committed yet has
        // nothing cacheable, and a new version simply misses.
        // Drop the lock before any await: the fallback below hits the network.
        let remembered = self.inodes.lock().seen.get(&ino).copied();
        let version_id = match remembered {
            Some((version, _)) => version,
            None => self.client.get_node(node_id).await?.current_version_id,
        };
        let Some(version_id) = version_id else {
            return self.client.read_file(node_id, offset, size).await;
        };

        let client = self.client.clone();
        let out = cache
            .read(node_id, version_id, offset, size, move |at, len| {
                let client = client.clone();
                async move { client.read_file_cacheable(node_id, at, len).await }
            })
            .await?;
        if (out.len() as u64) < size {
            tracing::debug!(
                ino,
                offset,
                asked = size,
                got = out.len(),
                %version_id,
                "short read"
            );
        }
        Ok(out)
    }

    /// Pull every file into the cache, for mirror mode.
    ///
    /// Runs in the background after mount: the filesystem is usable while it
    /// works, since anything not yet mirrored is simply fetched on demand. A
    /// failure on one file is logged and skipped rather than aborting the walk,
    /// because a single unreadable file should not leave the mirror unstarted.
    pub async fn prefetch_all(&self) -> Result<(u64, u64), ClientError> {
        let mut queue = vec![ROOT_INO];
        let (mut files, mut bytes) = (0u64, 0u64);

        while let Some(ino) = queue.pop() {
            let entries = self.readdir(ino).await?;
            for entry in entries {
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                if entry.kind == NodeKind::Directory {
                    queue.push(entry.ino);
                    continue;
                }

                let size = match self.getattr(entry.ino).await {
                    Ok(attr) => attr.size,
                    Err(e) => {
                        tracing::warn!(ino = entry.ino, "mirror: cannot stat: {e}");
                        continue;
                    }
                };
                let mut at = 0;
                while at < size {
                    if let Err(e) = self.read(entry.ino, at, BLOCK_SIZE).await {
                        tracing::warn!(ino = entry.ino, offset = at, "mirror: cannot read: {e}");
                        break;
                    }
                    at += BLOCK_SIZE;
                }
                files += 1;
                bytes += size;
            }
        }

        Ok((files, bytes))
    }

    /// Buffer a write, sending it once the run stops being contiguous or the
    /// buffer fills. Returns the count the kernel asked us to accept.
    pub async fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u64, ClientError> {
        let node_id = self.id_for(ino)?;
        {
            // Remember how far the file now reaches, before anything is sent.
            let mut inodes = self.inodes.lock();
            let reach = inodes.high_water.entry(ino).or_insert(0);
            *reach = (*reach).max(offset + data.len() as u64);
        }
        let mut pending = self.pending.lock().await;

        match pending.get_mut(&ino) {
            // The common case: this write continues where the last one ended.
            Some(buffered) if buffered.offset + buffered.data.len() as u64 == offset => {
                if let (Some(log), Some(path)) = (self.log.as_ref(), buffered.log_path.as_ref()) {
                    if let Err(e) = log.extend(path, data) {
                        tracing::warn!("cannot extend the write log: {e}");
                    }
                }
                buffered.data.extend_from_slice(data);
            }
            Some(_) => {
                // A seek breaks the run: send what we have, then start over.
                let buffered = pending.remove(&ino).expect("just matched");
                self.send(node_id, &buffered).await?;
                pending.insert(ino, self.start_run(node_id, offset, data));
            }
            None => {
                pending.insert(ino, self.start_run(node_id, offset, data));
            }
        }

        // Flush where the run ends on a part boundary, so the server can replace
        // whole parts. The size cap only catches a run that never reaches one.
        let buffered = &pending[&ino];
        let end = buffered.offset + buffered.data.len() as u64;
        let on_boundary = end % self.chunk_size == 0;
        let too_big = buffered.data.len() as u64 >= self.chunk_size.max(WRITE_BUFFER_BYTES) * 2;
        if on_boundary || too_big {
            let buffered = pending.remove(&ino).expect("just inserted");
            self.send(node_id, &buffered).await?;
        }

        if let Some(cache) = self.cache.as_ref() {
            cache.invalidate(node_id).await;
        }
        Ok(data.len() as u64)
    }

    /// Set attributes. A size change is a truncate: the server clamps reads to
    /// the new size, and the next write re-chunks from there.
    #[allow(clippy::too_many_arguments)]
    pub async fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        mtime: Option<DateTime<Utc>>,
        atime: Option<DateTime<Utc>>,
    ) -> Result<Attr, ClientError> {
        // A truncate must not race a buffered write of the old contents.
        self.flush(ino).await?;
        if let Some(size) = size {
            // A truncate is authoritative, including when it shrinks the file.
            self.inodes.lock().high_water.insert(ino, size);
        }
        let id = self.id_for(ino)?;
        if let Some(cache) = self.cache.as_ref() {
            cache.invalidate(id).await;
        }
        let node = self
            .client
            .patch_node(
                id,
                PatchNodeRequest {
                    mode,
                    uid,
                    gid,
                    size,
                    mtime,
                    atime,
                    idempotency_key: Uuid::new_v4(),
                },
            )
            .await?;
        Ok(self.attr_of(&node))
    }

    pub async fn fsync(&self, ino: u64) -> Result<(), ClientError> {
        self.flush(ino).await?;
        self.client.sync_file(self.id_for(ino)?).await
    }

    /// Remove one entry. Used for both `unlink` and `rmdir`; the server rejects
    /// a directory that still has children.
    pub async fn remove(&self, parent_ino: u64, name: &[u8]) -> Result<(), ClientError> {
        let target = self.lookup(parent_ino, name).await?;
        // Drop anything buffered for the victim rather than writing it back,
        // including its durable record.
        if let Some(dropped) = self.pending.lock().await.remove(&target.ino) {
            if let (Some(log), Some(path)) = (self.log.as_ref(), dropped.log_path.as_ref()) {
                log.finish(path);
            }
        }
        let id = self.id_for(target.ino)?;
        self.client.delete_node(id).await?;
        if let Some(cache) = self.cache.as_ref() {
            cache.invalidate(id).await;
        }
        // Drop the mapping so a recycled name gets a fresh inode.
        let mut inodes = self.inodes.lock();
        inodes.by_ino.remove(&target.ino);
        inodes.by_id.remove(&id);
        inodes.seen.remove(&target.ino);
        inodes.high_water.remove(&target.ino);
        Ok(())
    }

    /// Metadata-only move: no bytes are read, written or re-uploaded.
    pub async fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
    ) -> Result<(), ClientError> {
        let target = self.lookup(parent_ino, name).await?;
        let id = self.id_for(target.ino)?;
        let new_parent_id = self.id_for(new_parent_ino)?;

        // The destination is replaced, so anything cached or buffered for what
        // used to live there is now about to be wrong.
        if let Ok(replaced) = self.lookup(new_parent_ino, new_name).await {
            if replaced.ino != target.ino {
                if let Some(dropped) = self.pending.lock().await.remove(&replaced.ino) {
                    if let (Some(log), Some(path)) = (self.log.as_ref(), dropped.log_path.as_ref())
                    {
                        log.finish(path);
                    }
                }
                if let Ok(replaced_id) = self.id_for(replaced.ino) {
                    if let Some(cache) = self.cache.as_ref() {
                        cache.invalidate(replaced_id).await;
                    }
                    let mut inodes = self.inodes.lock();
                    inodes.by_ino.remove(&replaced.ino);
                    inodes.by_id.remove(&replaced_id);
                    inodes.seen.remove(&replaced.ino);
                    inodes.high_water.remove(&replaced.ino);
                }
            }
        }

        let new_name = NameBytes::new(new_name.to_vec()).map_err(|_| ClientError::InvalidName)?;
        self.client
            .rename_node(
                id,
                RenameNodeRequest {
                    new_parent_id,
                    new_name,
                    idempotency_key: Uuid::new_v4(),
                },
            )
            .await?;
        Ok(())
    }
}
