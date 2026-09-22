//! Byte-level read/write semantics — the path the FUSE client actually uses.

mod common;

use axum::http::StatusCode;
use common::{key, name, send, send_bytes};
use dcfs_server::{create_server, TEST_CHUNK_SIZE};
use serde_json::json;

const CS: u64 = TEST_CHUNK_SIZE;

/// Router plus one empty file and the root id.
async fn fixture() -> (axum::Router, String, String) {
    let router = create_server();
    let (_, root) = send(&router, "GET", "/api/v1/nodes/root", None).await;
    let root_id = root["id"].as_str().unwrap().to_string();
    let (status, file) = send(
        &router,
        "POST",
        "/api/v1/nodes",
        Some(json!({
            "parent_id": root_id, "name": name(b"data.bin"), "kind": "File",
            "mode": 0o100644, "uid": 1000, "gid": 1000, "idempotency_key": key(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{file}");
    (router, root_id, file["id"].as_str().unwrap().to_string())
}

async fn write_at(router: &axum::Router, file: &str, offset: u64, data: &[u8]) -> StatusCode {
    let (status, _) = send_bytes(
        router,
        "PUT",
        &format!("/api/v1/nodes/{file}/data?offset={offset}"),
        data.to_vec(),
    )
    .await;
    status
}

/// Close the file. A write builds one staging version and this commits it,
/// the same way the FUSE client's `flush` does.
async fn send_with_headers(
    router: &axum::Router,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap) {
    use tower::ServiceExt;
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    (response.status(), response.headers().clone())
}

async fn sync(router: &axum::Router, file: &str) {
    let (status, _) = send_bytes(
        router,
        "POST",
        &format!("/api/v1/nodes/{file}/sync"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

async fn read_all(router: &axum::Router, file: &str) -> Vec<u8> {
    let (status, body) = send_bytes(
        router,
        "GET",
        &format!("/api/v1/nodes/{file}/data"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

#[tokio::test]
async fn write_then_read_returns_the_same_bytes() {
    let (router, _, file) = fixture().await;

    assert_eq!(
        write_at(&router, &file, 0, b"hello world").await,
        StatusCode::OK
    );
    // Readable straight away, before the file is closed.
    assert_eq!(read_all(&router, &file).await, b"hello world".to_vec());
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(
        node["size"], 11,
        "the size is current while the write is open"
    );

    sync(&router, &file).await;
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["size"], 11);
    assert!(
        node["current_version_id"].is_string(),
        "closing the file commits the version"
    );
}

#[tokio::test]
async fn reading_an_empty_file_returns_nothing() {
    let (router, _, file) = fixture().await;
    assert!(read_all(&router, &file).await.is_empty());
}

#[tokio::test]
async fn a_write_spanning_chunk_boundaries_round_trips() {
    let (router, _, file) = fixture().await;

    // Two and a half chunks, with a distinct byte per position.
    let payload: Vec<u8> = (0..(CS * 2 + CS / 2)).map(|i| (i % 251) as u8).collect();
    assert_eq!(write_at(&router, &file, 0, &payload).await, StatusCode::OK);

    assert_eq!(read_all(&router, &file).await, payload);
    sync(&router, &file).await;

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let version = node["current_version_id"].as_str().unwrap();
    let (_, manifest) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{version}/chunks"),
        None,
    )
    .await;
    assert_eq!(
        manifest.as_array().unwrap().len(),
        3,
        "payload spans 3 chunks"
    );
}

#[tokio::test]
async fn an_unaligned_overwrite_preserves_the_surrounding_bytes() {
    let (router, _, file) = fixture().await;

    let original = vec![b'.'; (CS * 2) as usize];
    write_at(&router, &file, 0, &original).await;

    // Straddle the boundary between chunk 0 and chunk 1.
    let patch = b"XXXX";
    let offset = CS - 2;
    assert_eq!(
        write_at(&router, &file, offset, patch).await,
        StatusCode::OK
    );

    let mut expected = original.clone();
    expected[offset as usize..offset as usize + patch.len()].copy_from_slice(patch);
    assert_eq!(read_all(&router, &file).await, expected);

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(
        node["size"],
        CS * 2,
        "an in-place overwrite must not grow the file"
    );
}

#[tokio::test]
async fn untouched_chunks_are_carried_over_by_reference() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, &vec![b'a'; (CS * 3) as usize]).await;
    sync(&router, &file).await;
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let v1 = node["current_version_id"].as_str().unwrap().to_string();
    let (_, before) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{v1}/chunks"),
        None,
    )
    .await;

    // Touch only chunk 0.
    write_at(&router, &file, 0, b"z").await;
    sync(&router, &file).await;

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let v2 = node["current_version_id"].as_str().unwrap().to_string();
    let (_, after) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{v2}/chunks"),
        None,
    )
    .await;

    assert_ne!(
        before[0]["object_id"], after[0]["object_id"],
        "chunk 0 was rewritten"
    );
    assert_eq!(
        before[1]["object_id"], after[1]["object_id"],
        "chunk 1 must be reused, not re-uploaded"
    );
    assert_eq!(before[2]["object_id"], after[2]["object_id"]);
}

#[tokio::test]
async fn writing_past_the_end_leaves_a_zero_filled_hole() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, b"abc").await;
    // Land well past EOF, inside a chunk that does not exist yet.
    let offset = CS + 5;
    assert_eq!(
        write_at(&router, &file, offset, b"tail").await,
        StatusCode::OK
    );

    let content = read_all(&router, &file).await;
    assert_eq!(content.len() as u64, offset + 4);
    assert_eq!(&content[..3], b"abc");
    assert!(
        content[3..offset as usize].iter().all(|&b| b == 0),
        "the gap must read as zeroes"
    );
    assert_eq!(&content[offset as usize..], b"tail");
}

#[tokio::test]
async fn appending_extends_the_file() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, b"one").await;
    write_at(&router, &file, 3, b"two").await;
    write_at(&router, &file, 6, b"three").await;

    assert_eq!(read_all(&router, &file).await, b"onetwothree".to_vec());
}

#[tokio::test]
async fn partial_reads_are_clamped_to_the_file() {
    let (router, _, file) = fixture().await;
    write_at(&router, &file, 0, b"0123456789").await;

    let cases = [
        (0u64, 4u64, &b"0123"[..]),
        (6, 4, &b"6789"[..]),
        (8, 100, &b"89"[..]),
        (10, 5, &b""[..]),
        (99, 5, &b""[..]),
    ];
    for (offset, size, expected) in cases {
        let (status, body) = send_bytes(
            &router,
            "GET",
            &format!("/api/v1/nodes/{file}/data?offset={offset}&size={size}"),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, expected.to_vec(), "offset={offset} size={size}");
    }
}

#[tokio::test]
async fn an_empty_write_changes_nothing() {
    let (router, _, file) = fixture().await;
    write_at(&router, &file, 0, b"keep").await;

    assert_eq!(write_at(&router, &file, 0, b"").await, StatusCode::OK);
    assert_eq!(read_all(&router, &file).await, b"keep".to_vec());

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(
        node["generation"], 1,
        "an empty write must not stage a version"
    );
}

#[tokio::test]
async fn directories_reject_byte_io() {
    let (router, root_id, _) = fixture().await;

    let (status, _) = send_bytes(
        &router,
        "GET",
        &format!("/api/v1/nodes/{root_id}/data"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send_bytes(
        &router,
        "PUT",
        &format!("/api/v1/nodes/{root_id}/data?offset=0"),
        b"nope".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn io_on_an_unknown_node_is_not_found() {
    let router = create_server();
    let missing = uuid::Uuid::new_v4();

    for (method, uri) in [
        ("GET", format!("/api/v1/nodes/{missing}/data")),
        ("PUT", format!("/api/v1/nodes/{missing}/data?offset=0")),
        ("POST", format!("/api/v1/nodes/{missing}/sync")),
    ] {
        let (status, _) = send_bytes(&router, method, &uri, b"x".to_vec()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

#[tokio::test]
async fn sync_confirms_a_durable_write() {
    let (router, _, file) = fixture().await;
    write_at(&router, &file, 0, b"durable").await;

    let (status, _) = send_bytes(
        &router,
        "POST",
        &format!("/api/v1/nodes/{file}/sync"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(read_all(&router, &file).await, b"durable".to_vec());
}

/// Writing a file used to create one version per request, and each version
/// copied the manifest of the one before it, so an N-part file wrote
/// N(N+1)/2 chunk rows. One staging version for the whole write makes that N.
#[tokio::test]
async fn a_sequential_write_does_not_rewrite_its_manifest_every_time() {
    let (router, _, file) = fixture().await;

    const PARTS: u64 = 12;
    for index in 0..PARTS {
        assert_eq!(
            write_at(&router, &file, index * CS, &vec![b'a'; CS as usize]).await,
            StatusCode::OK
        );
    }
    sync(&router, &file).await;

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["size"], CS * PARTS);
    let version = node["current_version_id"].as_str().unwrap();

    // One version for the whole write, not one per part.
    assert_eq!(
        node["generation"], 1,
        "one staging version was opened, not {PARTS}"
    );

    // And its manifest has exactly one row per part.
    let (_, manifest) = send(
        &router,
        "GET",
        &format!("/api/v1/versions/{version}/chunks"),
        None,
    )
    .await;
    assert_eq!(manifest.as_array().unwrap().len() as u64, PARTS);

    // The bytes are still right.
    assert_eq!(read_all(&router, &file).await.len() as u64, CS * PARTS);
}

#[tokio::test]
async fn a_write_in_progress_is_readable_before_it_is_closed() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, b"partially written").await;
    // Not committed yet, but a reader must not see an empty file: the bytes
    // were accepted, so they have to be there.
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert!(
        node["current_version_id"].is_null(),
        "nothing committed yet"
    );
    assert_eq!(node["size"], 17);
    assert_eq!(
        read_all(&router, &file).await,
        b"partially written".to_vec()
    );

    // Continuing the write keeps working on the same version.
    write_at(&router, &file, 17, b" and more").await;
    assert_eq!(
        read_all(&router, &file).await,
        b"partially written and more".to_vec()
    );

    sync(&router, &file).await;
    assert_eq!(
        read_all(&router, &file).await,
        b"partially written and more".to_vec()
    );
}

#[tokio::test]
async fn closing_a_file_twice_is_harmless() {
    let (router, _, file) = fixture().await;
    write_at(&router, &file, 0, b"once").await;
    sync(&router, &file).await;
    // Nothing is open the second time; this must not fail or lose anything.
    sync(&router, &file).await;
    assert_eq!(read_all(&router, &file).await, b"once".to_vec());
}

/// A long upload that dies partway must be resumable: the bytes already
/// accepted stay, and writing on from the size the file reports finishes it.
/// Re-sending 30 GB because a copy broke three hours in is not an option.
#[tokio::test]
async fn an_interrupted_upload_carries_on_from_where_it_stopped() {
    let (router, _, file) = fixture().await;

    // Four parts in, then the writer disappears without closing the file.
    let first: Vec<u8> = (0..CS * 4).map(|i| (i % 251) as u8).collect();
    assert_eq!(write_at(&router, &file, 0, &first).await, StatusCode::OK);

    // A fresh client asks how far the file got.
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let resume_from = node["size"].as_u64().unwrap();
    assert_eq!(resume_from, CS * 4);

    // ...and writes on from exactly there.
    let rest: Vec<u8> = (CS * 4..CS * 9).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        write_at(&router, &file, resume_from, &rest).await,
        StatusCode::OK
    );
    sync(&router, &file).await;

    let whole: Vec<u8> = (0..CS * 9).map(|i| (i % 251) as u8).collect();
    assert_eq!(read_all(&router, &file).await, whole, "no hole at the seam");

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert_eq!(node["size"], CS * 9);
}

/// Resuming must land on a part boundary or not at all: continuing from a
/// size that falls inside a part still has to produce the right bytes.
#[tokio::test]
async fn a_resume_inside_a_part_still_joins_cleanly() {
    let (router, _, file) = fixture().await;

    let head_len = CS * 2 + CS / 3;
    let head: Vec<u8> = (0..head_len).map(|i| (i % 251) as u8).collect();
    write_at(&router, &file, 0, &head).await;

    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    let resume_from = node["size"].as_u64().unwrap();
    assert_eq!(
        resume_from, head_len,
        "an unaligned size is reported exactly"
    );

    let tail: Vec<u8> = (head_len..CS * 5).map(|i| (i % 251) as u8).collect();
    write_at(&router, &file, resume_from, &tail).await;
    sync(&router, &file).await;

    let whole: Vec<u8> = (0..CS * 5).map(|i| (i % 251) as u8).collect();
    assert_eq!(read_all(&router, &file).await, whole);
}

/// Two clients writing different parts of one file must both land. They share
/// the open write, so this is really asking whether the session that made a
/// sequential write cheap also kept concurrent writers from losing each other.
#[tokio::test]
async fn two_clients_writing_different_parts_both_land() {
    let (router, _, file) = fixture().await;

    let a: Vec<u8> = vec![b'a'; CS as usize];
    let b: Vec<u8> = vec![b'b'; CS as usize];
    let (first, second) = tokio::join!(
        write_at(&router, &file, 0, &a),
        write_at(&router, &file, CS * 4, &b),
    );
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    sync(&router, &file).await;

    let whole = read_all(&router, &file).await;
    assert_eq!(whole.len() as u64, CS * 5);
    assert_eq!(&whole[..CS as usize], &a[..], "the first client's part");
    assert_eq!(
        &whole[(CS * 4) as usize..],
        &b[..],
        "the second client's part"
    );
    // The gap between them was never written, so it reads as a hole.
    assert!(whole[CS as usize..(CS * 4) as usize]
        .iter()
        .all(|&x| x == 0));
}

/// Both writing the same place is last-write-wins, as it is on a local
/// filesystem — but it must be one of the two, never a mix of both.
#[tokio::test]
async fn two_clients_writing_the_same_part_do_not_interleave() {
    let (router, _, file) = fixture().await;

    let a: Vec<u8> = vec![b'a'; CS as usize];
    let b: Vec<u8> = vec![b'b'; CS as usize];
    let (first, second) = tokio::join!(
        write_at(&router, &file, 0, &a),
        write_at(&router, &file, 0, &b)
    );
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    sync(&router, &file).await;

    let whole = read_all(&router, &file).await;
    assert!(
        whole == a || whole == b,
        "one writer won outright, rather than the two being spliced together"
    );
}

/// A second client reads what a first has written but not yet closed. This is
/// what keeps a file being copied from reading as empty, and it means an open
/// write is visible to everyone, not isolated to the writer.
#[tokio::test]
async fn a_second_client_sees_an_unclosed_write() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, b"visible before close").await;
    let (_, node) = send(&router, "GET", &format!("/api/v1/nodes/{file}"), None).await;
    assert!(
        node["current_version_id"].is_null(),
        "nothing committed yet"
    );
    assert_eq!(
        read_all(&router, &file).await,
        b"visible before close".to_vec()
    );
}

/// A caching client has to be told when the bytes it just read can still
/// change, or it will keep serving content the file may never end up holding.
#[tokio::test]
async fn a_read_says_whether_its_bytes_are_settled() {
    let (router, _, file) = fixture().await;

    write_at(&router, &file, 0, b"still being written").await;
    let (status, headers) = send_with_headers(&router, &format!("/api/v1/nodes/{file}/data")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-dfs-committed").map(|v| v.to_str().unwrap()),
        Some("false"),
        "an open write must not be cached"
    );

    sync(&router, &file).await;
    let (_, headers) = send_with_headers(&router, &format!("/api/v1/nodes/{file}/data")).await;
    assert_eq!(
        headers.get("x-dfs-committed").map(|v| v.to_str().unwrap()),
        Some("true"),
        "a committed version is immutable and may be kept"
    );
}
