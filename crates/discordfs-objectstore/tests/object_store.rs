//! Object store contract tests.

use bytes::Bytes;
use discordfs_core::ObjectId;
use discordfs_objectstore::memory::MemoryObjectStore;
use discordfs_objectstore::{ObjectLocator, ObjectStore};

#[tokio::test]
async fn put_and_get_roundtrip() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let data = Bytes::from_static(b"hello world");

    let _: discordfs_objectstore::StoredObject = store.put(id, data.clone()).await.unwrap();

    let locator = ObjectLocator::new(id);
    let retrieved: Bytes = store.get(&locator).await.unwrap();
    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn put_rejects_duplicate_id() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let data1 = Bytes::from_static(b"first");
    let data2 = Bytes::from_static(b"second");

    let _: discordfs_objectstore::StoredObject = store.put(id, data1).await.unwrap();
    let result: Result<discordfs_objectstore::StoredObject, _> = store.put(id, data2).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn get_returns_not_found_for_missing() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let locator = ObjectLocator::new(id);

    let result: Result<Bytes, _> = store.get(&locator).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn delete_removes_object() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let data = Bytes::from_static(b"test");

    let _: discordfs_objectstore::StoredObject = store.put(id, data).await.unwrap();
    let locator = ObjectLocator::new(id);
    let _: () = store.delete(&locator).await.unwrap();

    let result: Result<Bytes, _> = store.get(&locator).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn delete_returns_not_found_for_missing() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let locator = ObjectLocator::new(id);

    let result: Result<(), _> = store.delete(&locator).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn stat_returns_metadata() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let data = Bytes::from_static(b"test data");

    let stored: discordfs_objectstore::StoredObject = store.put(id, data.clone()).await.unwrap();
    assert_eq!(stored.id, id);
    assert_eq!(stored.size, data.len() as u64);

    let locator = ObjectLocator::new(id);
    let stat: discordfs_objectstore::StoredObject = store.stat(&locator).await.unwrap();
    assert_eq!(stat.id, id);
    assert_eq!(stat.size, data.len() as u64);
}

#[tokio::test]
async fn stat_returns_not_found_for_missing() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let locator = ObjectLocator::new(id);

    let result: Result<discordfs_objectstore::StoredObject, _> = store.stat(&locator).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn objects_are_immutable() {
    let store = MemoryObjectStore::new();
    let id = ObjectId::new();
    let data = Bytes::from_static(b"immutable");

    let _: discordfs_objectstore::StoredObject = store.put(id, data.clone()).await.unwrap();

    // Cannot overwrite
    let result: Result<discordfs_objectstore::StoredObject, _> =
        store.put(id, Bytes::from_static(b"different")).await;
    assert!(result.is_err());

    // Original data still there
    let locator = ObjectLocator::new(id);
    let retrieved: Bytes = store.get(&locator).await.unwrap();
    assert_eq!(retrieved, data);
}
