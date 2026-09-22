//! Local content cache, in fixed-size blocks.
//!
//! Two modes, the same shape as Google Drive's:
//!
//! - **Stream**: fetch blocks on demand and keep the most recently used ones
//!   up to a byte budget. Disk usage is bounded no matter how large the
//!   filesystem is.
//! - **Mirror**: keep every block that is ever fetched, and prefetch the whole
//!   tree at mount, so reads are served locally and the mount keeps working
//!   through a server outage.
//!
//! Blocks are keyed by version, so a new committed version simply misses the
//! cache instead of serving stale bytes. Cached blocks are **plaintext** —
//! decryption happens on the server — so the cache directory holds readable
//! file contents and must be protected like the files themselves.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// How much of a file one cached block holds.
pub const BLOCK_SIZE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fetch on demand, evict least-recently-used blocks past the budget.
    Stream { budget_bytes: u64 },
    /// Keep everything, and pull the whole tree down at mount.
    Mirror,
}

impl Mode {
    pub fn is_mirror(&self) -> bool {
        matches!(self, Mode::Mirror)
    }
}

/// Identifies one block of one version of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    node_id: Uuid,
    version_id: Uuid,
    index: u64,
}

#[derive(Default)]
struct State {
    /// Block size on disk, plus the access counter that orders the LRU.
    blocks: HashMap<Key, (u64, u64)>,
    next_tick: u64,
    bytes: u64,
}

pub struct BlockCache {
    dir: PathBuf,
    mode: Mode,
    state: Mutex<State>,
}

impl BlockCache {
    /// Open the cache directory, discarding whatever a previous run left.
    ///
    /// ponytail: starting empty keeps the accounting honest without an index
    /// file to load and validate. A mirror re-fetches on the next prefetch;
    /// persisting the index across restarts is the upgrade path.
    pub async fn open(dir: impl Into<PathBuf>, mode: Mode) -> std::io::Result<Self> {
        let dir = dir.into();
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&dir).await?;
        }
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir,
            mode,
            state: Mutex::new(State::default()),
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn path_of(&self, key: &Key) -> PathBuf {
        self.dir
            .join(key.node_id.simple().to_string())
            .join(key.version_id.simple().to_string())
            .join(key.index.to_string())
    }

    /// Serve `offset..offset + size` of a version, fetching the blocks it spans
    /// through `fetch` when they are not cached.
    ///
    /// `fetch(block_offset, block_len)` must return exactly that range of the
    /// file, or fewer bytes at end of file.
    pub async fn read<F, Fut, E>(
        &self,
        node_id: Uuid,
        version_id: Uuid,
        offset: u64,
        size: u64,
        fetch: F,
    ) -> Result<Vec<u8>, E>
    where
        F: Fn(u64, u64) -> Fut,
        Fut: std::future::Future<Output = Result<(Vec<u8>, bool), E>>,
    {
        if size == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(size as usize);
        let first = offset / BLOCK_SIZE;
        let last = (offset + size - 1) / BLOCK_SIZE;

        for index in first..=last {
            let key = Key {
                node_id,
                version_id,
                index,
            };
            let block_start = index * BLOCK_SIZE;

            let block = match self.load(&key).await {
                Some(bytes) => bytes,
                None => {
                    // The fetch says whether this block came from something
                    // immutable. Bytes from a write still in progress are
                    // served but never kept: the same key would otherwise go
                    // on returning content the file may never end up holding.
                    let (bytes, keepable) = fetch(block_start, BLOCK_SIZE).await?;
                    if keepable {
                        self.store(&key, &bytes).await;
                    }
                    bytes
                }
            };

            // Clamp the wanted window to what this block actually holds; a
            // short block means end of file.
            let from = offset.saturating_sub(block_start).min(block.len() as u64);
            let to = (offset + size - block_start).min(block.len() as u64);
            out.extend_from_slice(&block[from as usize..to as usize]);
        }

        Ok(out)
    }

    /// Forget every block of a file. Called when it is written or removed, so
    /// the next read cannot serve what the write replaced.
    pub async fn invalidate(&self, node_id: Uuid) {
        {
            let mut state = self.state.lock();
            let doomed: Vec<Key> = state
                .blocks
                .keys()
                .filter(|key| key.node_id == node_id)
                .copied()
                .collect();
            for key in doomed {
                if let Some((size, _)) = state.blocks.remove(&key) {
                    state.bytes = state.bytes.saturating_sub(size);
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(self.dir.join(node_id.simple().to_string())).await;
    }

    /// Bytes currently held on disk.
    pub fn bytes(&self) -> u64 {
        self.state.lock().bytes
    }

    /// Number of cached blocks.
    pub fn block_count(&self) -> usize {
        self.state.lock().blocks.len()
    }

    async fn load(&self, key: &Key) -> Option<Vec<u8>> {
        {
            let mut state = self.state.lock();
            let tick = state.next_tick;
            let entry = state.blocks.get_mut(key)?;
            entry.1 = tick;
            state.next_tick += 1;
        }
        match tokio::fs::read(self.path_of(key)).await {
            Ok(bytes) if bytes.is_empty() => {
                // An empty block is never a real cache entry; treat it as a miss
                // rather than as end of file.
                let mut state = self.state.lock();
                if let Some((size, _)) = state.blocks.remove(key) {
                    state.bytes = state.bytes.saturating_sub(size);
                }
                None
            }
            Ok(bytes) => Some(bytes),
            Err(_) => {
                // The file vanished under us; drop the accounting and refetch.
                let mut state = self.state.lock();
                if let Some((size, _)) = state.blocks.remove(key) {
                    state.bytes = state.bytes.saturating_sub(size);
                }
                None
            }
        }
    }

    async fn store(&self, key: &Key, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let path = self.path_of(key);
        if let Some(parent) = path.parent() {
            if tokio::fs::create_dir_all(parent).await.is_err() {
                return;
            }
        }
        // A cache write that fails is a miss, never an error to the caller.
        //
        // The temporary name must be unique per write, not per block: the
        // kernel's readahead fetches two halves of the same block at once, and
        // a shared temp path let one writer rename the other's half-written
        // file into place. Reads then stopped at that short block and reported
        // EOF in the middle of a file, with nothing logged anywhere.
        let temp = path.with_extension(format!("partial-{}", Uuid::new_v4().simple()));
        if tokio::fs::write(&temp, bytes).await.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
            return;
        }
        if tokio::fs::rename(&temp, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
            return;
        }

        let evict = {
            let mut state = self.state.lock();
            let tick = state.next_tick;
            state.next_tick += 1;
            if let Some((old, _)) = state.blocks.insert(*key, (bytes.len() as u64, tick)) {
                state.bytes = state.bytes.saturating_sub(old);
            }
            state.bytes += bytes.len() as u64;

            match self.mode {
                // A mirror keeps everything: that is the whole point of it.
                Mode::Mirror => Vec::new(),
                Mode::Stream { budget_bytes } => Self::pick_victims(&mut state, budget_bytes),
            }
        };

        for key in evict {
            let _ = tokio::fs::remove_file(self.path_of(&key)).await;
        }
    }

    /// Choose least-recently-used blocks until the budget is met, dropping them
    /// from the accounting; the caller unlinks them.
    fn pick_victims(state: &mut State, budget_bytes: u64) -> Vec<Key> {
        if state.bytes <= budget_bytes {
            return Vec::new();
        }
        let mut by_age: Vec<(u64, Key)> = state
            .blocks
            .iter()
            .map(|(key, (_, tick))| (*tick, *key))
            .collect();
        by_age.sort_unstable_by_key(|(tick, _)| *tick);

        let mut victims = Vec::new();
        for (_, key) in by_age {
            if state.bytes <= budget_bytes {
                break;
            }
            if let Some((size, _)) = state.blocks.remove(&key) {
                state.bytes = state.bytes.saturating_sub(size);
                victims.push(key);
            }
        }
        victims
    }

    /// Remove the cache directory. Best effort, on unmount.
    pub async fn discard(&self) {
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn temp_cache(mode: Mode) -> BlockCache {
        let dir = std::env::temp_dir().join(format!("dfs-blocks-{}", Uuid::new_v4().simple()));
        BlockCache::open(dir, mode).await.unwrap()
    }

    /// A file of `size` bytes where byte i is (i % 251), plus a fetch counter.
    /// What a fetch hands back: the bytes, and whether they may be kept.
    type Fetched = (Vec<u8>, bool);

    fn source(
        size: u64,
    ) -> (
        impl Fn(u64, u64) -> futures_lite_future<Fetched>,
        Arc<AtomicUsize>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fetch = move |offset: u64, len: u64| {
            counter.fetch_add(1, Ordering::SeqCst);
            let end = (offset + len).min(size);
            let bytes: Vec<u8> = (offset..end).map(|i| (i % 251) as u8).collect();
            futures_lite_future(Ok((bytes, true)))
        };
        (fetch, calls)
    }

    /// A ready-made future, so the tests do not need an async-closure crate.
    #[allow(non_camel_case_types)]
    type futures_lite_future<T> = std::future::Ready<Result<T, std::convert::Infallible>>;

    fn futures_lite_future<T>(
        value: Result<T, std::convert::Infallible>,
    ) -> futures_lite_future<T> {
        std::future::ready(value)
    }

    #[tokio::test]
    async fn a_cached_block_is_fetched_once() {
        let cache = temp_cache(Mode::Mirror).await;
        let (fetch, calls) = source(BLOCK_SIZE * 2);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        let first = cache.read(node, version, 0, 100, &fetch).await.unwrap();
        assert_eq!(first.len(), 100);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A second read inside the same block must not go back to the server.
        let second = cache.read(node, version, 50, 50, &fetch).await.unwrap();
        assert_eq!(second, first[50..100]);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "served from cache");

        cache.discard().await;
    }

    #[tokio::test]
    async fn a_read_spanning_blocks_stitches_them_together() {
        let cache = temp_cache(Mode::Mirror).await;
        let (fetch, calls) = source(BLOCK_SIZE * 3);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        let start = BLOCK_SIZE - 10;
        let got = cache.read(node, version, start, 20, &fetch).await.unwrap();
        let want: Vec<u8> = (start..start + 20).map(|i| (i % 251) as u8).collect();
        assert_eq!(got, want);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "two blocks touched");

        cache.discard().await;
    }

    #[tokio::test]
    async fn a_short_final_block_is_not_padded() {
        let cache = temp_cache(Mode::Mirror).await;
        let size = BLOCK_SIZE + 7;
        let (fetch, _) = source(size);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        let got = cache
            .read(node, version, BLOCK_SIZE, 1000, &fetch)
            .await
            .unwrap();
        assert_eq!(got.len(), 7, "end of file is end of file");

        cache.discard().await;
    }

    #[tokio::test]
    async fn stream_mode_evicts_to_stay_inside_its_budget() {
        // Room for two blocks.
        let cache = temp_cache(Mode::Stream {
            budget_bytes: BLOCK_SIZE * 2,
        })
        .await;
        let (fetch, _) = source(BLOCK_SIZE * 10);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        for index in 0..5u64 {
            cache
                .read(node, version, index * BLOCK_SIZE, 10, &fetch)
                .await
                .unwrap();
        }

        assert!(
            cache.bytes() <= BLOCK_SIZE * 2,
            "cache grew past its budget: {} bytes",
            cache.bytes()
        );
        assert_eq!(cache.block_count(), 2);

        cache.discard().await;
    }

    #[tokio::test]
    async fn mirror_mode_keeps_everything() {
        let cache = temp_cache(Mode::Mirror).await;
        let (fetch, _) = source(BLOCK_SIZE * 5);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        for index in 0..5u64 {
            cache
                .read(node, version, index * BLOCK_SIZE, 10, &fetch)
                .await
                .unwrap();
        }
        assert_eq!(cache.block_count(), 5, "a mirror never evicts");

        cache.discard().await;
    }

    #[tokio::test]
    async fn a_new_version_does_not_read_the_old_bytes() {
        let cache = temp_cache(Mode::Mirror).await;
        let (node, v1, v2) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

        let old = |_: u64, _: u64| {
            std::future::ready(Ok::<_, std::convert::Infallible>((b"old".to_vec(), true)))
        };
        let new = |_: u64, _: u64| {
            std::future::ready(Ok::<_, std::convert::Infallible>((b"new".to_vec(), true)))
        };

        assert_eq!(cache.read(node, v1, 0, 3, &old).await.unwrap(), b"old");
        // Same file, different version: the key changes, so nothing stale can
        // be served even though the old blocks are still on disk.
        assert_eq!(cache.read(node, v2, 0, 3, &new).await.unwrap(), b"new");

        cache.discard().await;
    }

    /// The kernel's readahead asks for two halves of the same block at once.
    /// Both miss, both fetch, both store — and a shared temporary file let one
    /// overwrite the other's, leaving a short block that read back as EOF.
    #[tokio::test]
    async fn concurrent_fetches_of_one_block_do_not_corrupt_it() {
        let cache = Arc::new(temp_cache(Mode::Mirror).await);
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());

        let mut tasks = Vec::new();
        for start in [0u64, BLOCK_SIZE / 2] {
            let cache = cache.clone();
            tasks.push(tokio::spawn(async move {
                let fetch = |offset: u64, len: u64| {
                    let end = (offset + len).min(BLOCK_SIZE);
                    std::future::ready(Ok::<_, std::convert::Infallible>((
                        (offset..end).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
                        true,
                    )))
                };
                cache
                    .read(node, version, start, BLOCK_SIZE / 2, &fetch)
                    .await
                    .unwrap()
            }));
        }

        for (index, task) in tasks.into_iter().enumerate() {
            let got = task.await.unwrap();
            let start = index as u64 * (BLOCK_SIZE / 2);
            let want: Vec<u8> = (start..start + BLOCK_SIZE / 2)
                .map(|i| (i % 251) as u8)
                .collect();
            assert_eq!(got.len(), want.len(), "half {index} came back short");
            assert_eq!(got, want);
        }

        // And the block left on disk is whole, so later reads are not short.
        let fetch = |_: u64, _: u64| {
            std::future::ready(Ok::<_, std::convert::Infallible>((vec![0u8; 0], true)))
        };
        let again = cache
            .read(node, version, 0, BLOCK_SIZE, &fetch)
            .await
            .unwrap();
        assert_eq!(
            again.len(),
            BLOCK_SIZE as usize,
            "cached block is truncated"
        );

        cache.discard().await;
    }

    /// Bytes from a write still in progress are served but never kept: the
    /// version id does not change while the write goes on, so a cached block
    /// would keep answering with content the file may never end up holding.
    #[tokio::test]
    async fn bytes_from_an_open_write_are_not_kept() {
        let cache = temp_cache(Mode::Mirror).await;
        let (node, version) = (Uuid::new_v4(), Uuid::new_v4());
        let calls = Arc::new(AtomicUsize::new(0));

        let counter = calls.clone();
        let in_progress = move |_: u64, _: u64| {
            counter.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok::<_, std::convert::Infallible>((
                b"early".to_vec(),
                false,
            )))
        };
        assert_eq!(
            cache.read(node, version, 0, 5, &in_progress).await.unwrap(),
            b"early".to_vec()
        );
        assert_eq!(
            cache.read(node, version, 0, 5, &in_progress).await.unwrap(),
            b"early".to_vec()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "fetched again, not cached");
        assert_eq!(cache.block_count(), 0);

        // Once the write is closed the same key may be kept.
        let settled = |_: u64, _: u64| {
            std::future::ready(Ok::<_, std::convert::Infallible>((b"final".to_vec(), true)))
        };
        assert_eq!(
            cache.read(node, version, 0, 5, &settled).await.unwrap(),
            b"final".to_vec()
        );
        assert_eq!(cache.block_count(), 1);
    }

    #[tokio::test]
    async fn invalidate_drops_every_block_of_one_file() {
        let cache = temp_cache(Mode::Mirror).await;
        let (fetch, calls) = source(BLOCK_SIZE * 2);
        let (node, other, version) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

        cache.read(node, version, 0, 10, &fetch).await.unwrap();
        cache.read(other, version, 0, 10, &fetch).await.unwrap();
        assert_eq!(cache.block_count(), 2);

        cache.invalidate(node).await;
        assert_eq!(cache.block_count(), 1, "only the named file is dropped");
        // A whole block is cached even when the read wanted 10 bytes of it.
        assert_eq!(cache.bytes(), BLOCK_SIZE);

        let before = calls.load(Ordering::SeqCst);
        cache.read(node, version, 0, 10, &fetch).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before + 1,
            "the invalidated block must be refetched"
        );

        cache.discard().await;
    }
}
