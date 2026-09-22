//! Chunk specification and planning.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error when planning chunks.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ChunkPlanError {
    #[error("chunk size must be greater than zero")]
    ZeroChunkSize,
    #[error("file size overflow")]
    SizeOverflow,
}

/// Specification for a single chunk within a file version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSpec {
    /// Zero-based chunk index.
    pub index: u64,
    /// Logical offset within the file.
    pub offset: u64,
    /// Size of this chunk in bytes.
    pub len: u64,
}

impl ChunkSpec {
    /// Get the end offset (exclusive) of this chunk.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }
}

/// Plan chunks for a file of the given size with the specified chunk size.
///
/// Returns a vector of `ChunkSpec` describing each chunk's index, offset, and length.
/// The last chunk may be smaller than `chunk_size` if the file size is not evenly divisible.
///
/// # Errors
///
/// Returns an error if `chunk_size` is zero.
pub fn plan_chunks(file_size: u64, chunk_size: u64) -> Result<Vec<ChunkSpec>, ChunkPlanError> {
    if chunk_size == 0 {
        return Err(ChunkPlanError::ZeroChunkSize);
    }

    if file_size == 0 {
        return Ok(vec![]);
    }

    let num_chunks = file_size.div_ceil(chunk_size);
    let mut chunks = Vec::with_capacity(num_chunks as usize);

    for i in 0..num_chunks {
        let offset = i
            .checked_mul(chunk_size)
            .ok_or(ChunkPlanError::SizeOverflow)?;
        let remaining = file_size
            .checked_sub(offset)
            .ok_or(ChunkPlanError::SizeOverflow)?;
        let len = remaining.min(chunk_size);

        chunks.push(ChunkSpec {
            index: i,
            offset,
            len,
        });
    }

    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_chunk_size_rejected() {
        assert_eq!(plan_chunks(100, 0), Err(ChunkPlanError::ZeroChunkSize));
    }

    #[test]
    fn zero_file_size_returns_empty() {
        let chunks = plan_chunks(0, 1024).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn exact_multiple_chunks() {
        let chunks = plan_chunks(1024, 256).unwrap();
        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks[0],
            ChunkSpec {
                index: 0,
                offset: 0,
                len: 256
            }
        );
        assert_eq!(
            chunks[1],
            ChunkSpec {
                index: 1,
                offset: 256,
                len: 256
            }
        );
        assert_eq!(
            chunks[2],
            ChunkSpec {
                index: 2,
                offset: 512,
                len: 256
            }
        );
        assert_eq!(
            chunks[3],
            ChunkSpec {
                index: 3,
                offset: 768,
                len: 256
            }
        );
    }

    #[test]
    fn unaligned_last_chunk() {
        let chunks = plan_chunks(17, 8).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0],
            ChunkSpec {
                index: 0,
                offset: 0,
                len: 8
            }
        );
        assert_eq!(
            chunks[1],
            ChunkSpec {
                index: 1,
                offset: 8,
                len: 8
            }
        );
        assert_eq!(
            chunks[2],
            ChunkSpec {
                index: 2,
                offset: 16,
                len: 1
            }
        );
    }

    #[test]
    fn single_small_file() {
        let chunks = plan_chunks(100, 1024).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            ChunkSpec {
                index: 0,
                offset: 0,
                len: 100
            }
        );
    }

    #[test]
    fn chunk_end_is_correct() {
        let chunk = ChunkSpec {
            index: 0,
            offset: 100,
            len: 50,
        };
        assert_eq!(chunk.end(), 150);
    }

    #[test]
    fn chunks_cover_entire_file() {
        let file_size = 12345;
        let chunk_size = 1024;
        let chunks = plan_chunks(file_size, chunk_size).unwrap();

        let total_len: u64 = chunks.iter().map(|c| c.len).sum();
        assert_eq!(total_len, file_size);

        // Verify no gaps
        for i in 1..chunks.len() {
            assert_eq!(chunks[i].offset, chunks[i - 1].end());
        }
    }

    #[test]
    fn large_file_plans_correctly() {
        let file_size = 100 * 1024 * 1024; // 100 MB
        let chunk_size = 8 * 1024 * 1024; // 8 MB
        let chunks = plan_chunks(file_size, chunk_size).unwrap();
        assert_eq!(chunks.len(), 13); // 100 / 8 = 12.5, so 13 chunks
    }
}
