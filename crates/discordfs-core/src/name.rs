//! Linux filename byte validation.
//!
//! Linux filenames are arbitrary bytes except for NUL (0x00) and '/' (0x2F).
//! This module provides a validated `NodeName` type that enforces these rules.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error when creating an invalid node name.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum NodeNameError {
    #[error("filename contains NUL byte")]
    ContainsNul,
    #[error("filename contains slash")]
    ContainsSlash,
    #[error("filename is empty")]
    Empty,
}

/// A validated Linux filename as raw bytes.
///
/// Rejects NUL (0x00) and '/' (0x2F) bytes. Empty names are also rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeName(Vec<u8>);

impl NodeName {
    /// Create a new validated node name from raw bytes.
    ///
    /// Returns an error if the name contains NUL or '/' bytes, or is empty.
    pub fn new(bytes: Vec<u8>) -> Result<Self, NodeNameError> {
        if bytes.is_empty() {
            return Err(NodeNameError::Empty);
        }
        if bytes.contains(&0x00) {
            return Err(NodeNameError::ContainsNul);
        }
        if bytes.contains(&b'/') {
            return Err(NodeNameError::ContainsSlash);
        }
        Ok(Self(bytes))
    }

    /// Get the raw bytes of the filename.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Convert to a lossy UTF-8 string for display purposes.
    ///
    /// This is NOT authoritative for filesystem operations.
    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    /// Consume self and return the underlying bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for NodeName {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        assert!(NodeName::new(b"hello".to_vec()).is_ok());
        assert!(NodeName::new(b"file.txt".to_vec()).is_ok());
        assert!(NodeName::new(b".hidden".to_vec()).is_ok());
        assert!(NodeName::new(b"with spaces".to_vec()).is_ok());
    }

    #[test]
    fn accepts_non_utf8_names() {
        // Valid non-UTF8 bytes (Latin-1 encoded "café")
        let bytes = vec![0x63, 0x61, 0x66, 0xe9];
        assert!(NodeName::new(bytes).is_ok());
    }

    #[test]
    fn rejects_slash() {
        assert_eq!(
            NodeName::new(b"a/b".to_vec()),
            Err(NodeNameError::ContainsSlash)
        );
    }

    #[test]
    fn rejects_nul() {
        assert_eq!(
            NodeName::new(b"a\0b".to_vec()),
            Err(NodeNameError::ContainsNul)
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(NodeName::new(vec![]), Err(NodeNameError::Empty));
    }

    #[test]
    fn as_bytes_returns_raw() {
        let name = NodeName::new(b"test".to_vec()).unwrap();
        assert_eq!(name.as_bytes(), b"test");
    }

    #[test]
    fn to_string_lossy_handles_non_utf8() {
        let bytes = vec![0x63, 0x61, 0x66, 0xe9];
        let name = NodeName::new(bytes).unwrap();
        let lossy = name.to_string_lossy();
        // Should contain replacement character or be lossy
        assert!(!lossy.is_empty());
    }
}
