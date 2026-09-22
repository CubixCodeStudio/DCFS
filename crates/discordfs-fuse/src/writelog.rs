//! Durable record of writes that have been accepted but not yet sent.
//!
//! Writes are coalesced in memory before they reach the server, so a `write`
//! returns success while the bytes are still only in this process. If it dies
//! there, the file quietly loses them. Each buffered run is therefore also
//! written to disk and noted in a journal, and anything the journal still holds
//! at the next mount is replayed before the filesystem serves anyone.
//!
//! The bytes are plaintext, like the block cache, so the directory holding them
//! needs the same protection as the files themselves.
//!
//! ponytail: the data file is written but not fsynced, so this survives the
//! process dying, not the machine losing power. Add an fsync per run — not per
//! write — if that matters more than the throughput it costs.

use discordfs_cache::{Journal, JournalEntry};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// One run of bytes waiting to be sent.
pub struct PendingRun {
    pub node_id: Uuid,
    pub offset: u64,
    pub data: Vec<u8>,
}

pub struct WriteLog {
    dir: PathBuf,
    journal: Journal,
}

impl WriteLog {
    /// Open the log, leaving anything a previous run left behind to be
    /// recovered by [`WriteLog::take_pending`].
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let journal = Journal::new(&dir);
        Ok(Self { dir, journal })
    }

    /// Start recording a run, returning the file its bytes go to.
    pub fn begin(&self, node_id: Uuid, offset: u64, data: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.dir.join(format!("{}.run", Uuid::new_v4().simple()));
        std::fs::write(&path, data)?;
        self.journal.record_dirty(&JournalEntry {
            node_id,
            base_version_id: None,
            generation: 0,
            local_path: path.clone(),
            offset,
            logical_size: data.len() as u64,
            dirty: true,
        })?;
        Ok(path)
    }

    /// Add to a run already being recorded. The run is contiguous, so this is
    /// an append.
    pub fn extend(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(data)
    }

    /// Drop a run once the server has it.
    pub fn finish(&self, path: &Path) {
        // A leftover file is replayed harmlessly at the next mount, so failing
        // to remove it is not worth reporting.
        let _ = std::fs::remove_file(path);
    }

    /// Everything a previous run left unsent, oldest first.
    ///
    /// Entries whose data file is gone were completed: the file is removed once
    /// the server has the bytes, and the journal line outlives it.
    pub fn take_pending(&self) -> std::io::Result<Vec<PendingRun>> {
        let mut runs = Vec::new();
        for entry in self.journal.recover()? {
            match std::fs::read(&entry.local_path) {
                Ok(data) if !data.is_empty() => runs.push(PendingRun {
                    node_id: entry.node_id,
                    offset: entry.offset,
                    data,
                }),
                _ => continue,
            }
        }
        Ok(runs)
    }

    /// Forget every record. Called once recovery has replayed them.
    pub fn reset(&self) -> std::io::Result<()> {
        for entry in self.journal.recover().unwrap_or_default() {
            let _ = std::fs::remove_file(&entry.local_path);
        }
        self.journal.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("dfs-writelog-{}", Uuid::new_v4().simple()))
    }

    #[test]
    fn a_run_is_replayable_until_it_is_finished() {
        let dir = temp_dir();
        let log = WriteLog::open(&dir).unwrap();
        let node = Uuid::new_v4();

        let path = log.begin(node, 4096, b"first").unwrap();
        log.extend(&path, b"second").unwrap();

        // Whatever is still recorded is what a crash here would have lost.
        let reopened = WriteLog::open(&dir).unwrap();
        let pending = reopened.take_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_id, node);
        assert_eq!(pending[0].offset, 4096);
        assert_eq!(pending[0].data, b"firstsecond");

        // Once the server has it, nothing is left to replay.
        log.finish(&path);
        assert!(WriteLog::open(&dir)
            .unwrap()
            .take_pending()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn several_runs_replay_in_the_order_they_were_written() {
        let dir = temp_dir();
        let log = WriteLog::open(&dir).unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        log.begin(a, 0, b"aaa").unwrap();
        log.begin(b, 64, b"bbb").unwrap();

        let pending = WriteLog::open(&dir).unwrap().take_pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].node_id, a);
        assert_eq!(pending[1].node_id, b);
        assert_eq!(pending[1].offset, 64);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_clears_the_records_and_their_files() {
        let dir = temp_dir();
        let log = WriteLog::open(&dir).unwrap();
        let path = log.begin(Uuid::new_v4(), 0, b"gone").unwrap();

        log.reset().unwrap();
        assert!(!path.exists());
        assert!(log.take_pending().unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
