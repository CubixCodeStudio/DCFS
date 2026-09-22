//! Chunk encryption and integrity operations for DiscordFS.
//!
//! Uses XChaCha20-Poly1305 AEAD with BLAKE3 hashing.

pub mod chunk;

pub use chunk::*;
