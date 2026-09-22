//! Chunk crypto integration tests.

use dcfs_core::ObjectId;
use dcfs_crypto::{open, seal, CryptoError, EncryptionKey, KeyId};

#[test]
fn roundtrip_preserves_data() {
    let key_id = KeyId::new("key-1");
    let key = EncryptionKey::from_bytes([0xAB; 32]);
    let object_id = ObjectId::new();
    let plaintext = b"test data for encryption";

    let encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();
    let decrypted = open(&key, &encrypted).unwrap();

    assert_eq!(decrypted, plaintext);
}

#[test]
fn tampered_ciphertext_rejected() {
    let key_id = KeyId::new("key-1");
    let key = EncryptionKey::from_bytes([0xAB; 32]);
    let object_id = ObjectId::new();
    let plaintext = b"test data";

    let mut encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();
    // Tamper with ciphertext
    if let Some(byte) = encrypted.ciphertext.last_mut() {
        *byte ^= 0xFF;
    }

    let result = open(&key, &encrypted);
    assert!(matches!(
        result,
        Err(CryptoError::IntegrityCheckFailed { .. })
    ));
}

#[test]
fn wrong_key_rejected() {
    let key_id = KeyId::new("key-1");
    let key1 = EncryptionKey::from_bytes([0xAB; 32]);
    let key2 = EncryptionKey::from_bytes([0xCD; 32]);
    let object_id = ObjectId::new();
    let plaintext = b"test data";

    let encrypted = seal(&key_id, &key1, object_id, plaintext).unwrap();
    let result = open(&key2, &encrypted);
    assert!(matches!(result, Err(CryptoError::DecryptionFailed)));
}

#[test]
fn object_id_mismatch_rejected() {
    let key_id = KeyId::new("key-1");
    let key = EncryptionKey::from_bytes([0xAB; 32]);
    let object_id1 = ObjectId::new();
    let object_id2 = ObjectId::new();
    let plaintext = b"test data";

    let mut encrypted = seal(&key_id, &key, object_id1, plaintext).unwrap();
    encrypted.object_id = object_id2;

    let result = open(&key, &encrypted);
    assert!(result.is_err());
}

#[test]
fn hashes_are_populated() {
    let key_id = KeyId::new("key-1");
    let key = EncryptionKey::from_bytes([0xAB; 32]);
    let object_id = ObjectId::new();
    let plaintext = b"test data";

    let encrypted = seal(&key_id, &key, object_id, plaintext).unwrap();

    assert!(!encrypted.plaintext_hash.is_empty());
    assert!(!encrypted.ciphertext_hash.is_empty());
    assert_ne!(encrypted.plaintext_hash, encrypted.ciphertext_hash);
}
