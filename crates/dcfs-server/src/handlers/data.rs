//! Byte-level file I/O.
//!
//! The FUSE client sends plain bytes and never sees a chunk, an object id or a
//! key: chunking, encryption and the version commit all happen here, so the
//! Discord token and the master key stay on the server.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use dcfs_core::ObjectId;
use dcfs_crypto::{hash_data, open, open_range, seal, ChunkRange, EncryptedChunk};
use dcfs_db::{CommitGuard, FileChunkRecord, RepositoryError};
use dcfs_objectstore::ObjectLocator;
use serde::Deserialize;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

/// Tells a caching client whether these bytes came from a version that can
/// still change.
///
/// A committed version is immutable, so a client may keep its blocks. A write
/// in progress is not: the same version id can serve different bytes as the
/// write goes on, and if it is abandoned the file reverts to what was
/// committed. Caching that would leave a client holding content that never
/// existed.
const COMMITTED_HEADER: &str = "x-dfs-committed";
const COMMITTED_HEADERS: [(&str, &str); 1] = [(COMMITTED_HEADER, "true")];
const UNCOMMITTED_HEADERS: [(&str, &str); 1] = [(COMMITTED_HEADER, "false")];

/// Commit a write in progress each time the file grows past a multiple of
/// this.
///
/// Every commit starts a fresh staging version, which copies the manifest
/// once, so committing often brings back the cost this design exists to avoid.
/// Committing rarely leaves more to lose if the writer disappears, and leaves
/// a staging version looking abandoned for longer. 1 GiB is a compromise.
const COMMIT_EVERY_BYTES: u64 = 1024 * 1024 * 1024;

/// How many of a read's parts to fetch at once.
///
/// ponytail: a fixed cap, not a tuned pool. Raise it if a backend's latency
/// dominates and it can take the concurrency.
const MAX_PARALLEL_CHUNK_FETCHES: usize = 8;

#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    pub offset: Option<u64>,
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct WriteQuery {
    pub offset: Option<u64>,
}

/// Fetch one chunk's object and return its verified plaintext.
///
/// `ciphertext_hash` is recomputed from the bytes we just fetched, so it only
/// guards against corruption inside this function; the real integrity check is
/// the AEAD tag plus `plaintext_hash`, which comes from the authoritative
/// metadata store rather than from the object itself.
async fn read_chunk_plaintext(
    state: &AppState,
    chunk: &FileChunkRecord,
) -> Result<Vec<u8>, AppError> {
    let locator = ObjectLocator::new(ObjectId::from_uuid(chunk.object_id));
    let ciphertext = state.store.get(&locator).await?;
    let envelope = EncryptedChunk {
        key_id: state.key_id.clone(),
        object_id: ObjectId::from_uuid(chunk.object_id),
        plaintext_hash: chunk.plaintext_hash.clone(),
        ciphertext_hash: hash_data(&ciphertext),
        ciphertext: ciphertext.to_vec(),
    };
    open(&state.key, &envelope).map_err(|e| {
        // Integrity failures are a corruption signal, not a client mistake.
        tracing::error!(object_id = %chunk.object_id, "chunk failed to decrypt: {e}");
        AppError::internal("chunk integrity failure")
    })
}

/// Read `from..to` of one chunk's plaintext, fetching only the segments that
/// cover it.
///
/// A chunk is sealed as independently authenticated segments, so a small read
/// costs one segment rather than the whole chunk. Objects written before that
/// layout have a single tag over everything and cannot be opened in part, so
/// they fall back to the whole-chunk read.
async fn read_chunk_slice(
    state: &AppState,
    chunk: &FileChunkRecord,
    from: usize,
    to: usize,
) -> Result<Vec<u8>, AppError> {
    let len = chunk.plaintext_size.max(0) as usize;
    let want = to.saturating_sub(from);
    let readable = to.min(len);
    if from >= readable {
        // Entirely past this chunk's plaintext: the caller zero-fills.
        return Ok(vec![0u8; want]);
    }

    let locator = ObjectLocator::new(ObjectId::from_uuid(chunk.object_id));
    let range = ChunkRange {
        object_id: ObjectId::from_uuid(chunk.object_id),
        plaintext_len: len,
        from,
        to: readable,
    };
    let (start, end, first_segment) = range.to_fetch();
    let fetched = state
        .store
        .get_range(&locator, start as u64, (end - start) as u64)
        .await?;

    let mut plaintext = match open_range(&state.key, &state.key_id, &range, &fetched, first_segment)
    {
        Ok(plaintext) => plaintext,
        Err(_) => {
            // Either an object in the old layout, or real corruption. Reading
            // the whole chunk tells the two apart and reports it properly.
            let whole = read_chunk_plaintext(state, chunk).await?;
            whole
                .get(from..readable.min(whole.len()))
                .unwrap_or_default()
                .to_vec()
        }
    };

    // A short chunk reads as zeroes past its end, as POSIX expects of a hole.
    plaintext.resize(want, 0);
    Ok(plaintext)
}

/// Seal a chunk under a fresh object id and store it immutably.
pub async fn write_chunk_plaintext(
    state: &AppState,
    plaintext: &[u8],
    file: Uuid,
) -> Result<(Uuid, String), AppError> {
    let object_id = ObjectId::new();
    let sealed = seal(&state.key_id, &state.key, object_id, plaintext)
        .map_err(|e| AppError::internal(format!("encryption failed: {e}")))?;
    // Everything belonging to one file goes to one place. A backend spread
    // over several endpoints keeps a file whole that way; the rest ignore it.
    state
        .store
        .put_for(object_id, sealed.ciphertext.clone().into(), file)
        .await?;
    Ok((*object_id.as_uuid(), sealed.plaintext_hash))
}

/// `GET /api/v1/nodes/:id/data?offset=&size=`
pub async fn read_data(
    State(state): State<AppState>,
    Path(node_id): Path<Uuid>,
    Query(query): Query<ReadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let node = state.repo.get_node(node_id).await?;
    if node.kind == "directory" {
        return Err(AppError::bad_request("cannot read a directory"));
    }

    let offset = query.offset.unwrap_or(0);
    let file_size = node.size.max(0) as u64;
    if offset >= file_size {
        return Ok((StatusCode::OK, COMMITTED_HEADERS, Bytes::new()));
    }
    let size = query
        .size
        .unwrap_or(file_size - offset)
        .min(file_size - offset);
    if size == 0 {
        return Ok((StatusCode::OK, COMMITTED_HEADERS, Bytes::new()));
    }

    // A write in progress builds one staging version and commits it on sync,
    // so that is where the newest bytes are. Reading only the committed
    // version would show a file that is being copied as empty.
    let (version, committed) = match state.repo.find_open_staging_version(node_id).await {
        Ok(working) => (working, false),
        Err(RepositoryError::NotFound) => {
            let Some(version_id) = node.current_version_id else {
                // Size without a version means nothing exists yet.
                return Ok((StatusCode::OK, COMMITTED_HEADERS, Bytes::new()));
            };
            (state.repo.get_version(version_id).await?, true)
        }
        Err(e) => return Err(e.into()),
    };
    let headers = if committed {
        COMMITTED_HEADERS
    } else {
        UNCOMMITTED_HEADERS
    };
    let version_id = version.id;
    let chunk_size = version.chunk_size.max(1) as u64;

    let first = offset / chunk_size;
    let last = (offset + size - 1) / chunk_size;
    // Only the chunks this range covers: a read costs the same whether the file
    // is a kilobyte or a terabyte.
    let chunks = state
        .repo
        .get_chunk_range(version_id, first as i64, last as i64)
        .await?;

    // Fetch the parts a read spans at the same time. Over a backend where
    // every part is a network round trip, doing them one after another makes
    // a read as slow as the sum of its parts rather than the slowest of them.
    let mut pieces: Vec<Vec<u8>> = vec![Vec::new(); (last - first + 1) as usize];
    let mut inflight = tokio::task::JoinSet::new();
    let mut next = first;

    while next <= last || !inflight.is_empty() {
        while next <= last && inflight.len() < MAX_PARALLEL_CHUNK_FETCHES {
            let chunk_start = next * chunk_size;
            let want_from = offset.saturating_sub(chunk_start) as usize;
            let want_to = ((offset + size).min(chunk_start + chunk_size) - chunk_start) as usize;
            let slot = (next - first) as usize;

            // A hole means the version is incomplete; read zeroes rather than
            // silently shifting later bytes into the gap.
            match chunks.iter().find(|c| c.chunk_index as u64 == next) {
                Some(chunk) => {
                    let state = state.clone();
                    let chunk = chunk.clone();
                    inflight.spawn(async move {
                        (
                            slot,
                            read_chunk_slice(&state, &chunk, want_from, want_to).await,
                        )
                    });
                }
                None => pieces[slot] = vec![0u8; want_to - want_from],
            }
            next += 1;
        }

        if let Some(done) = inflight.join_next().await {
            let (slot, piece) = done.map_err(|e| AppError::internal(format!("read task: {e}")))?;
            pieces[slot] = piece?;
        }
    }

    let mut out = Vec::with_capacity(size as usize);
    for piece in pieces {
        out.extend_from_slice(&piece);
    }

    Ok((StatusCode::OK, headers, Bytes::from(out)))
}

/// `PUT /api/v1/nodes/:id/data?offset=` with the raw bytes as the body.
///
/// Writes are read-modify-write at chunk granularity: only the chunks the write
/// touches are re-encrypted and re-uploaded, the rest are carried over by
/// reference, and the whole thing becomes visible through one atomic commit.
pub async fn write_data(
    State(state): State<AppState>,
    Path(node_id): Path<Uuid>,
    Query(query): Query<WriteQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let offset = query.offset.unwrap_or(0);
    if body.is_empty() {
        return Ok((StatusCode::OK, "0".to_string()));
    }

    // Serialize writers to this file: everything below reads the node, adds to
    // its working version and records how long the file now is. The in-process
    // mutex keeps this instance's own requests apart without a round trip; the
    // database lock covers the other instances.
    let lock = state.write_lock(node_id);
    let _local = lock.lock().await;
    let _shared = state.repo.lock_node(node_id).await?;

    let node = state.repo.get_node(node_id).await?;
    if node.kind == "directory" {
        return Err(AppError::bad_request("cannot write to a directory"));
    }

    // A write in progress keeps one staging version and adds to it. Committing
    // per request meant every request copied the whole manifest of the one
    // before it, so an N-part file wrote N(N+1)/2 chunk rows instead of N.
    let working = match state.repo.find_open_staging_version(node_id).await {
        Ok(version) => version,
        Err(RepositoryError::NotFound) => {
            let base = node.current_version_id;
            let chunk_size = match base {
                Some(id) => state.repo.get_version(id).await?.chunk_size.max(1),
                None => state.chunk_size as i64,
            };
            let version = state
                .repo
                .create_staging_version(node_id, base, chunk_size)
                .await?;
            // Carry the committed manifest over once, at the start of the
            // write rather than on every request within it.
            if let Some(base) = base {
                let parts = (node.size.max(0) as u64).div_ceil(chunk_size.max(1) as u64);
                state
                    .repo
                    .copy_chunk_range(base, version.id, 0, parts as i64)
                    .await?;
            }
            version
        }
        Err(e) => return Err(e.into()),
    };

    let chunk_size = working.chunk_size.max(1) as u64;
    let old_size = node.size.max(0) as u64;
    let new_size = old_size.max(offset + body.len() as u64);
    tracing::debug!(%node_id, offset, len = body.len(), old_size, new_size, "write");
    let chunk_count = new_size.div_ceil(chunk_size);

    let write_first = offset / chunk_size;
    let write_last = (offset + body.len() as u64 - 1) / chunk_size;

    // The file's last chunk grows from partial to full when the file gets
    // longer, so it has to be rewritten even if the write does not reach it.
    let stale_tail = (old_size > 0 && old_size % chunk_size != 0).then(|| old_size / chunk_size);
    let rewrite_last = match stale_tail {
        Some(tail) if tail < chunk_count && (tail < write_first || tail > write_last) => Some(tail),
        _ => None,
    };

    // Read only what this write merges into: the parts it overlaps, plus that
    // tail. Everything else is already in the working manifest, untouched.
    let mut old_chunks = state
        .repo
        .get_chunk_range(
            working.id,
            write_first as i64,
            write_last.min(chunk_count) as i64,
        )
        .await?;
    if let Some(tail) = rewrite_last {
        old_chunks.extend(
            state
                .repo
                .get_chunk_range(working.id, tail as i64, tail as i64)
                .await?,
        );
    }

    let touched = (write_first..=write_last.min(chunk_count.saturating_sub(1)))
        .chain(rewrite_last)
        .collect::<std::collections::BTreeSet<_>>();

    for index in touched {
        let chunk_start = index * chunk_size;
        let chunk_len = chunk_size.min(new_size - chunk_start);
        let existing = old_chunks.iter().find(|c| c.chunk_index as u64 == index);

        let overlap_start = offset.max(chunk_start);
        let overlap_end = (offset + body.len() as u64).min(chunk_start + chunk_len);
        let covers_whole_chunk =
            overlap_start == chunk_start && overlap_end == chunk_start + chunk_len;

        // A write that covers the whole part replaces it outright, so there is
        // nothing to merge into: skip the fetch and the decrypt.
        let mut plaintext = if covers_whole_chunk {
            Vec::with_capacity(chunk_len as usize)
        } else {
            match existing {
                Some(chunk) => read_chunk_plaintext(&state, chunk).await?,
                // Writing past the end leaves a zero-filled hole, as POSIX expects.
                None => Vec::new(),
            }
        };
        plaintext.resize(chunk_len as usize, 0);

        if overlap_start < overlap_end {
            plaintext[(overlap_start - chunk_start) as usize..(overlap_end - chunk_start) as usize]
                .copy_from_slice(
                    &body[(overlap_start - offset) as usize..(overlap_end - offset) as usize],
                );
        }

        let (object_id, plaintext_hash) =
            write_chunk_plaintext(&state, &plaintext, node_id).await?;
        state
            .repo
            .attach_chunk(
                working.id,
                index as i64,
                chunk_start as i64,
                plaintext.len() as i32,
                &plaintext_hash,
                object_id,
            )
            .await?;
    }

    // The file is this long from now on, whether or not the write has been
    // committed yet: `stat` has to tell the truth while a copy is running.
    state
        .repo
        .touch_staging_version(working.id, new_size as i64)
        .await?;
    state
        .repo
        .update_node_attr(node_id, None, None, None, Some(new_size as i64), None, None)
        .await?;

    // Commit each time the file grows past another boundary. Counting bytes
    // per session would mean holding that count somewhere, and anywhere a
    // second server cannot see is the wrong place; the sizes say the same
    // thing and are already in the database.
    if new_size / COMMIT_EVERY_BYTES > old_size / COMMIT_EVERY_BYTES {
        commit_working_version(&state, node_id).await?;
    }

    Ok((StatusCode::OK, body.len().to_string()))
}

/// Commit the version a write has been building, if there is one.
///
/// Returns whether anything was committed.
async fn commit_working_version(state: &AppState, node_id: Uuid) -> Result<bool, AppError> {
    let working = match state.repo.find_open_staging_version(node_id).await {
        Ok(version) => version,
        Err(RepositoryError::NotFound) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let node = state.repo.get_node(node_id).await?;

    // A cheap identity for the version, not a content hash of the file:
    // computing that would mean reading every part back.
    let total_hash = hash_data(format!("{}:{}", working.id, working.size).as_bytes());
    state
        .repo
        .commit_version(
            CommitGuard {
                node_id,
                version_id: working.id,
                expected_generation: node.generation,
                expected_current_version: node.current_version_id,
            },
            working.size,
            &total_hash,
        )
        .await?;
    Ok(true)
}

/// `POST /api/v1/nodes/:id/sync`
///
/// Writes are already durable when `write_data` returns, so this only confirms
/// the node still exists. It stays in the API because the FUSE client calls it
/// on `fsync`, and a future write-back cache will have real work to do here.
pub async fn sync_node(
    State(state): State<AppState>,
    Path(node_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    state.repo.get_node(node_id).await?;
    // Writers add to one staging version and this is what makes it the file.
    let lock = state.write_lock(node_id);
    let _local = lock.lock().await;
    let _shared = state.repo.lock_node(node_id).await?;
    commit_working_version(&state, node_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
