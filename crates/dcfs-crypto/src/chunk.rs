//! Chunk encryption and integrity verification.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use dcfs_core::ObjectId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error from crypto operations.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encryption failed")]
    EncryptionFailed,
    #[error("decryption failed: authentication error")]
    DecryptionFailed,
    #[error("integrity check failed: expected {expected}, got {actual}")]
    IntegrityCheckFailed { expected: String, actual: String },
    #[error("invalid key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("object ID mismatch")]
    ObjectIdMismatch,
}

/// Key identifier for tracking which key encrypted an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyId(pub String);

impl KeyId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Encryption key (32 bytes for XChaCha20-Poly1305).
pub struct EncryptionKey([u8; 32]);

impl EncryptionKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Envelope containing encrypted chunk metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedChunk {
    /// Key ID used for encryption.
    pub key_id: KeyId,
    /// Object ID this chunk is bound to.
    pub object_id: ObjectId,
    /// BLAKE3 hash of plaintext.
    pub plaintext_hash: String,
    /// BLAKE3 hash of ciphertext.
    pub ciphertext_hash: String,
    /// Encrypted data.
    pub ciphertext: Vec<u8>,
}

/// How much plaintext one independently sealed segment holds.
///
/// A chunk is sealed as a run of these rather than as one AEAD message, so
/// reading a few bytes out of the middle of a large chunk costs one segment
/// instead of the whole thing. Smaller segments make a small read cheaper and
/// cost 16 more bytes each; 256 KiB puts the overhead of a 16 MiB chunk at
/// about a kilobyte while cutting the read amplification of a 4 KiB read from
/// 4096x to 64x.
pub const SEGMENT_SIZE: usize = 256 * 1024;

/// Marks the segmented layout. Anything without it is read as the original
/// single-AEAD format.
const MAGIC: &[u8; 4] = b"DFS2";
/// Magic plus the segment size the object was written with.
const HEADER_LEN: usize = 8;
const TAG_LEN: usize = 16;
/// The counter and last-segment flag occupy the tail of the 24-byte nonce.
const NONCE_PREFIX_LEN: usize = 19;

/// Bytes a sealed chunk of this size adds to its plaintext.
///
/// Callers sizing chunks against a backend's maximum upload size must leave
/// room for this.
pub fn ciphertext_overhead(plaintext_len: u64) -> u64 {
    HEADER_LEN as u64 + segment_count(plaintext_len as usize) as u64 * TAG_LEN as u64
}

/// The worst case of [`ciphertext_overhead`] for chunks up to 16 MiB, for
/// callers that need a constant.
pub const CIPHERTEXT_OVERHEAD: u64 =
    HEADER_LEN as u64 + (16 * 1024 * 1024 / SEGMENT_SIZE) as u64 * TAG_LEN as u64;

/// How many segments a plaintext of this length seals into. Empty plaintext
/// still has one, so that an empty chunk is distinguishable from a truncated
/// one.
pub fn segment_count(plaintext_len: usize) -> usize {
    plaintext_len.div_ceil(SEGMENT_SIZE).max(1)
}

/// Where a segment's sealed bytes sit inside the object.
fn segment_span(plaintext_len: usize, index: usize) -> (usize, usize) {
    let start = HEADER_LEN + index * (SEGMENT_SIZE + TAG_LEN);
    let plain = SEGMENT_SIZE.min(plaintext_len.saturating_sub(index * SEGMENT_SIZE));
    (start, start + plain + TAG_LEN)
}

/// A range of one chunk's plaintext, and where it lives inside the object.
#[derive(Debug, Clone, Copy)]
pub struct ChunkRange {
    pub object_id: ObjectId,
    /// Length of the chunk's plaintext, which fixes the segment layout.
    pub plaintext_len: usize,
    pub from: usize,
    pub to: usize,
}

impl ChunkRange {
    /// The object's byte range to fetch, and the index of the first segment
    /// in it. Callers pass both back to [`open_range`].
    pub fn to_fetch(&self) -> (usize, usize, usize) {
        let last_index = segment_count(self.plaintext_len) - 1;
        let first = (self.from / SEGMENT_SIZE).min(last_index);
        let last = (self.to.saturating_sub(1) / SEGMENT_SIZE).min(last_index);
        (
            segment_span(self.plaintext_len, first).0,
            segment_span(self.plaintext_len, last).1,
            first,
        )
    }
}

/// Each segment gets its own nonce, derived rather than stored: the object id
/// is unique and an object is never sealed twice, so the prefix is unique per
/// key, and the counter makes it unique per segment. The last-segment flag is
/// what stops a truncated object from opening as a shorter, valid one.
fn nonce_for(key_id: &KeyId, object_id: ObjectId, index: u32, is_last: bool) -> [u8; 24] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(object_id.as_uuid().as_bytes());
    hasher.update(key_id.0.as_bytes());
    let derived = hasher.finalize();

    let mut nonce = [0u8; 24];
    nonce[..NONCE_PREFIX_LEN].copy_from_slice(&derived.as_bytes()[..NONCE_PREFIX_LEN]);
    nonce[NONCE_PREFIX_LEN..NONCE_PREFIX_LEN + 4].copy_from_slice(&index.to_le_bytes());
    nonce[23] = u8::from(is_last);
    nonce
}

fn aad_of(key_id: &KeyId, object_id: ObjectId) -> Vec<u8> {
    let mut aad = Vec::new();
    aad.extend_from_slice(object_id.as_uuid().as_bytes());
    aad.extend_from_slice(key_id.0.as_bytes());
    aad
}

/// Decrypt the segments covering `from..to` and return exactly that range.
///
/// `fetched` is the object's bytes from [`ChunkRange::to_fetch`], and `first_index`
/// the index it reported. Each segment carries its own tag, so a range is
/// authenticated by the segments it touches — not against the whole chunk's
/// plaintext hash, which would mean reading the whole chunk to check.
pub fn open_range(
    key: &EncryptionKey,
    key_id: &KeyId,
    range: &ChunkRange,
    fetched: &[u8],
    first_index: usize,
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_bytes()));
    let aad = aad_of(key_id, range.object_id);
    let last_index = segment_count(range.plaintext_len) - 1;

    let mut plaintext = Vec::with_capacity(range.to - range.from);
    let mut cursor = 0usize;
    let mut index = first_index;
    while cursor < fetched.len() {
        let (start, end) = segment_span(range.plaintext_len, index);
        let len = end - start;
        if cursor + len > fetched.len() {
            return Err(CryptoError::DecryptionFailed);
        }
        let opened = cipher
            .decrypt(
                XNonce::from_slice(&nonce_for(
                    key_id,
                    range.object_id,
                    index as u32,
                    index == last_index,
                )),
                chacha20poly1305::aead::Payload {
                    msg: &fetched[cursor..cursor + len],
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::DecryptionFailed)?;
        plaintext.extend_from_slice(&opened);
        cursor += len;
        index += 1;
    }

    // What was fetched starts at a segment boundary, which is at or before the
    // first byte asked for.
    let skip = range.from - first_index * SEGMENT_SIZE;
    let want = range.to - range.from;
    if skip + want > plaintext.len() {
        return Err(CryptoError::DecryptionFailed);
    }
    Ok(plaintext[skip..skip + want].to_vec())
}

/// Compute BLAKE3 hash of data.
pub fn hash_data(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Encrypt a chunk as a run of independently authenticated segments.
///
/// The object ID and key ID are bound as associated data, and each segment's
/// nonce carries its index and whether it is the last, so segments cannot be
/// reordered, dropped or moved between objects.
pub fn seal(
    key_id: &KeyId,
    key: &EncryptionKey,
    object_id: ObjectId,
    plaintext: &[u8],
) -> Result<EncryptedChunk, CryptoError> {
    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_bytes()));
    let aad = aad_of(key_id, object_id);
    let segments = segment_count(plaintext.len());

    let mut full_ciphertext = Vec::with_capacity(HEADER_LEN + plaintext.len() + segments * TAG_LEN);
    full_ciphertext.extend_from_slice(MAGIC);
    full_ciphertext.extend_from_slice(&(SEGMENT_SIZE as u32).to_le_bytes());

    for index in 0..segments {
        let start = index * SEGMENT_SIZE;
        let end = (start + SEGMENT_SIZE).min(plaintext.len());
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce_for(
                    key_id,
                    object_id,
                    index as u32,
                    index == segments - 1,
                )),
                chacha20poly1305::aead::Payload {
                    msg: &plaintext[start..end],
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::EncryptionFailed)?;
        full_ciphertext.extend_from_slice(&sealed);
    }

    Ok(EncryptedChunk {
        key_id: key_id.clone(),
        object_id,
        plaintext_hash: hash_data(plaintext),
        ciphertext_hash: hash_data(&full_ciphertext),
        ciphertext: full_ciphertext,
    })
}

/// Decrypt and verify a chunk.
///
/// Verifies the object ID binding and plaintext hash.
pub fn open(key: &EncryptionKey, encrypted: &EncryptedChunk) -> Result<Vec<u8>, CryptoError> {
    let actual_hash = hash_data(&encrypted.ciphertext);
    if actual_hash != encrypted.ciphertext_hash {
        return Err(CryptoError::IntegrityCheckFailed {
            expected: encrypted.ciphertext_hash.clone(),
            actual: actual_hash,
        });
    }

    let plaintext = if encrypted.ciphertext.starts_with(MAGIC) {
        open_segmented(key, encrypted)?
    } else {
        // Objects written before the segmented layout: one AEAD message with
        // its nonce in front.
        open_single(key, encrypted)?
    };

    let actual_plaintext_hash = hash_data(&plaintext);
    if actual_plaintext_hash != encrypted.plaintext_hash {
        return Err(CryptoError::IntegrityCheckFailed {
            expected: encrypted.plaintext_hash.clone(),
            actual: actual_plaintext_hash,
        });
    }
    Ok(plaintext)
}

fn open_segmented(key: &EncryptionKey, encrypted: &EncryptedChunk) -> Result<Vec<u8>, CryptoError> {
    if encrypted.ciphertext.len() < HEADER_LEN {
        return Err(CryptoError::DecryptionFailed);
    }
    let segment_size = u32::from_le_bytes(encrypted.ciphertext[4..8].try_into().unwrap()) as usize;
    if segment_size != SEGMENT_SIZE {
        // Only the current segment size is produced; a different one means an
        // object this build cannot lay out.
        return Err(CryptoError::DecryptionFailed);
    }

    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_bytes()));
    let aad = aad_of(&encrypted.key_id, encrypted.object_id);

    let body = &encrypted.ciphertext[HEADER_LEN..];
    let segments = body.len().div_ceil(SEGMENT_SIZE + TAG_LEN).max(1);

    let mut plaintext = Vec::with_capacity(body.len());
    for index in 0..segments {
        let start = index * (SEGMENT_SIZE + TAG_LEN);
        let end = (start + SEGMENT_SIZE + TAG_LEN).min(body.len());
        let opened = cipher
            .decrypt(
                XNonce::from_slice(&nonce_for(
                    &encrypted.key_id,
                    encrypted.object_id,
                    index as u32,
                    index == segments - 1,
                )),
                chacha20poly1305::aead::Payload {
                    msg: &body[start..end],
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::DecryptionFailed)?;
        plaintext.extend_from_slice(&opened);
    }
    Ok(plaintext)
}

fn open_single(key: &EncryptionKey, encrypted: &EncryptedChunk) -> Result<Vec<u8>, CryptoError> {
    if encrypted.ciphertext.len() < 24 {
        return Err(CryptoError::DecryptionFailed);
    }
    let (nonce_bytes, ciphertext) = encrypted.ciphertext.split_at(24);
    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_bytes()));
    cipher
        .decrypt(
            XNonce::from_slice(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: &aad_of(&encrypted.key_id, encrypted.object_id),
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overhead_matches_what_seal_actually_adds() {
        let key = EncryptionKey::from_bytes([3u8; 32]);
        for len in [
            0usize,
            1,
            4096,
            SEGMENT_SIZE,
            SEGMENT_SIZE + 1,
            SEGMENT_SIZE * 4,
        ] {
            let sealed = seal(&KeyId::new("k"), &key, ObjectId::new(), &vec![0xab; len]).unwrap();
            assert_eq!(
                sealed.ciphertext.len() as u64,
                len as u64 + ciphertext_overhead(len as u64),
                "chunk sizing depends on this being exact"
            );
        }
    }

    /// Chunk sizes are checked against a backend's upload limit using the
    /// constant, so it must not undercount any chunk that fits in one.
    #[test]
    fn the_constant_covers_every_chunk_up_to_16_mib() {
        for len in [0u64, 1, 4096, 8 << 20, 16 << 20] {
            assert!(ciphertext_overhead(len) <= CIPHERTEXT_OVERHEAD, "len {len}");
        }
    }

    #[test]
    fn seal_and_open_roundtrip() {
        let key_id = KeyId::new("test-key-1");
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let object_id = ObjectId::new();
        let plaintext = b"hello world";

        let encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();
        let decrypted = open(&key, &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key_id = KeyId::new("test-key-1");
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let object_id = ObjectId::new();
        let plaintext = b"hello world";

        let mut encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();
        // Tamper with ciphertext
        if let Some(byte) = encrypted.ciphertext.last_mut() {
            *byte ^= 0xFF;
        }

        let result = open(&key, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let key_id = KeyId::new("test-key-1");
        let key1 = EncryptionKey::from_bytes([1u8; 32]);
        let key2 = EncryptionKey::from_bytes([2u8; 32]);
        let object_id = ObjectId::new();
        let plaintext = b"hello world";

        let encrypted = seal(&key_id, &key1, object_id, plaintext).unwrap();
        let result = open(&key2, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn object_id_binding() {
        let key_id = KeyId::new("test-key-1");
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let object_id1 = ObjectId::new();
        let object_id2 = ObjectId::new();
        let plaintext = b"hello world";

        let mut encrypted = seal(&key_id, &key, object_id1, plaintext).unwrap();
        // Change object ID in envelope (simulating metadata tampering)
        encrypted.object_id = object_id2;

        let result = open(&key, &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn empty_plaintext_works() {
        let key_id = KeyId::new("test-key-1");
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let object_id = ObjectId::new();
        let plaintext = b"";

        let encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();
        let decrypted = open(&key, &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn large_plaintext_works() {
        let key_id = KeyId::new("test-key-1");
        let key = EncryptionKey::from_bytes([1u8; 32]);
        let object_id = ObjectId::new();
        let plaintext = vec![42u8; 1024 * 1024]; // 1 MB

        let encrypted = seal(&key_id, &key, object_id, &plaintext).unwrap();
        let decrypted = open(&key, &encrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    fn key() -> EncryptionKey {
        EncryptionKey::from_bytes([7u8; 32])
    }

    /// Ranges must come back byte-identical wherever they fall, especially
    /// across a segment boundary, where the read spans two tags.
    #[test]
    fn a_range_matches_the_same_slice_of_the_plaintext() {
        let id = ObjectId::new();
        let kid = KeyId::new("k1");
        let plaintext: Vec<u8> = (0..(SEGMENT_SIZE * 3 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let sealed = seal(&kid, &key(), id, &plaintext).unwrap();

        for (from, to) in [
            (0, 10),
            (SEGMENT_SIZE - 5, SEGMENT_SIZE + 5),
            (SEGMENT_SIZE, SEGMENT_SIZE + 1),
            (SEGMENT_SIZE * 2 + 7, SEGMENT_SIZE * 2 + 9000),
            (plaintext.len() - 1, plaintext.len()),
            (0, plaintext.len()),
        ] {
            let range = ChunkRange {
                object_id: id,
                plaintext_len: plaintext.len(),
                from,
                to,
            };
            let (start, end, first) = range.to_fetch();
            let got =
                open_range(&key(), &kid, &range, &sealed.ciphertext[start..end], first).unwrap();
            assert_eq!(got, plaintext[from..to], "range {from}..{to}");
        }
    }

    /// The point of the layout: a small read touches one segment's worth of
    /// bytes, not the whole object.
    #[test]
    fn a_small_read_fetches_one_segment() {
        let len = SEGMENT_SIZE * 64;
        let (start, end, _) = ChunkRange {
            object_id: ObjectId::new(),
            plaintext_len: len,
            from: len / 2,
            to: len / 2 + 4096,
        }
        .to_fetch();
        assert_eq!(end - start, SEGMENT_SIZE + TAG_LEN);
    }

    #[test]
    fn a_tampered_segment_does_not_open() {
        let id = ObjectId::new();
        let kid = KeyId::new("k1");
        let plaintext = vec![9u8; SEGMENT_SIZE * 2];
        let mut sealed = seal(&kid, &key(), id, &plaintext).unwrap();
        sealed.ciphertext[HEADER_LEN + 100] ^= 1;

        let range = ChunkRange {
            object_id: id,
            plaintext_len: plaintext.len(),
            from: 0,
            to: 16,
        };
        let (start, end, first) = range.to_fetch();
        assert!(open_range(&key(), &kid, &range, &sealed.ciphertext[start..end], first).is_err());
    }

    /// Segments are bound to their position, so swapping two of them is not a
    /// valid object even though every tag is individually genuine.
    #[test]
    fn segments_cannot_be_reordered() {
        let id = ObjectId::new();
        let kid = KeyId::new("k1");
        let plaintext: Vec<u8> = (0..SEGMENT_SIZE * 2).map(|i| (i % 251) as u8).collect();
        let sealed = seal(&kid, &key(), id, &plaintext).unwrap();

        let unit = SEGMENT_SIZE + TAG_LEN;
        let mut swapped = sealed.ciphertext[..HEADER_LEN].to_vec();
        swapped.extend_from_slice(&sealed.ciphertext[HEADER_LEN + unit..HEADER_LEN + unit * 2]);
        swapped.extend_from_slice(&sealed.ciphertext[HEADER_LEN..HEADER_LEN + unit]);

        let tampered = EncryptedChunk {
            ciphertext_hash: hash_data(&swapped),
            ciphertext: swapped,
            ..sealed
        };
        assert!(open(&key(), &tampered).is_err());
    }

    /// Dropping the tail must not open as a shorter, valid chunk.
    #[test]
    fn a_truncated_object_does_not_open() {
        let id = ObjectId::new();
        let kid = KeyId::new("k1");
        let plaintext = vec![3u8; SEGMENT_SIZE * 3];
        let sealed = seal(&kid, &key(), id, &plaintext).unwrap();

        let cut = sealed.ciphertext[..HEADER_LEN + SEGMENT_SIZE + TAG_LEN].to_vec();
        let tampered = EncryptedChunk {
            ciphertext_hash: hash_data(&cut),
            ciphertext: cut,
            ..sealed
        };
        assert!(open(&key(), &tampered).is_err());
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let id = ObjectId::new();
        let kid = KeyId::new("k1");
        let sealed = seal(&kid, &key(), id, b"").unwrap();
        assert_eq!(open(&key(), &sealed).unwrap(), Vec::<u8>::new());
    }
}
