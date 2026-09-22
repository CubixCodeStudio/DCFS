//! Crash recovery journal.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Journal entry for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub node_id: Uuid,
    pub base_version_id: Option<Uuid>,
    pub generation: u64,
    /// File holding the bytes this entry describes.
    pub local_path: PathBuf,
    /// Where those bytes belong in the file.
    #[serde(default)]
    pub offset: u64,
    pub logical_size: u64,
    pub dirty: bool,
}

/// Journal for tracking dirty cache entries.
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(journal_dir: &Path) -> Self {
        Self {
            path: journal_dir.join("cache_journal.jsonl"),
        }
    }

    /// Record a dirty entry.
    pub fn record_dirty(&self, entry: &JournalEntry) -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        writeln!(file, "{}", line)?;
        Ok(())
    }

    /// Recover dirty entries from journal.
    pub fn recover(&self) -> std::io::Result<Vec<JournalEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&self.path)?;
        let reader = std::io::BufReader::new(file);
        let mut entries = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str(&line) {
                entries.push(entry);
            }
        }

        Ok(entries)
    }

    /// Where the journal file lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Clear the journal after successful recovery.
    pub fn clear(&self) -> std::io::Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn journal_roundtrip() {
        let dir = TempDir::new().unwrap();
        let journal = Journal::new(dir.path());

        let entry = JournalEntry {
            node_id: Uuid::new_v4(),
            base_version_id: None,
            generation: 1,
            local_path: PathBuf::from("/tmp/cache/test"),
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
    fn journal_clear() {
        let dir = TempDir::new().unwrap();
        let journal = Journal::new(dir.path());

        let entry = JournalEntry {
            node_id: Uuid::new_v4(),
            base_version_id: None,
            generation: 1,
            local_path: PathBuf::from("/tmp/cache/test"),
            offset: 0,
            logical_size: 1024,
            dirty: true,
        };

        journal.record_dirty(&entry).unwrap();
        journal.clear().unwrap();
        let recovered = journal.recover().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn empty_journal_returns_empty() {
        let dir = TempDir::new().unwrap();
        let journal = Journal::new(dir.path());
        let recovered = journal.recover().unwrap();
        assert!(recovered.is_empty());
    }
}
