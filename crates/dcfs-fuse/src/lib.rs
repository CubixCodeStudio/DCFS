//! DCFS FUSE filesystem implementation.
//!
//! This crate provides the client abstraction, handle management, and errno mapping
//! for mounting a DCFS backend as a local filesystem.

pub mod blockcache;
pub mod client;
pub mod errno;
pub mod fake_client;
pub mod handle;
pub mod service;
pub mod writelog;

#[cfg(target_os = "linux")]
pub mod fs;

pub use blockcache::{BlockCache, Mode, BLOCK_SIZE};
pub use client::{ClientError, HttpClient, ServerClient};
pub use errno::to_errno;
pub use handle::HandleTable;
pub use service::{Attr, DirEntry, Fs, FsStats, ROOT_INO};
pub use writelog::WriteLog;

#[cfg(target_os = "linux")]
pub use fs::DiscordFs;
