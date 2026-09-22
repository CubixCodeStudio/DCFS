# DCFS Rust v0.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a clean-room Rust v0.1 Linux FUSE filesystem with local write-back caching, transactional immutable file versions, encrypted chunk storage, PostgreSQL metadata, and a Discord attachment object-store backend.

**Architecture:** Implement vertical slices through independent Rust crates. FUSE never talks to Discord directly: it writes to a local cache, then uses a versioned HTTP protocol to commit immutable chunks through the server. The server owns metadata transactions, encryption, object-store access, Discord rate-limit handling, GC, and recovery; PostgreSQL is authoritative for committed namespace/version state.

**Tech Stack:** Rust stable (edition 2024 if supported by the installed stable toolchain, otherwise edition 2021), Tokio, Axum, reqwest, SQLx/PostgreSQL, fuser/FUSE3, serde, UUID, BLAKE3, XChaCha20-Poly1305, clap, tracing, Prometheus-compatible metrics.

**Spec:** `docs/superpowers/specs/2026-09-17-dcfs-rust-design.md`

## Global Constraints

- Linux/FUSE3 is the only v0.1 mount target.
- No DISFS source code may be copied; implementation is clean-room.
- Discord is a constrained backend: obey 429 `Retry-After`, use bounded concurrency, and never rotate bots/tokens/channels to evade limits.
- Default chunk payload is 8 MiB and is configurable; no Discord maximum is encoded into durable filesystem metadata.
- Remote chunks are immutable; there is no overwrite operation in `ObjectStore`.
- Every changed chunk is BLAKE3 hashed, encrypted with XChaCha20-Poly1305 using a fresh nonce, and authenticated before plaintext is returned.
- Discord bot token and database credentials never reach the FUSE client.
- A file version becomes visible only through one atomic metadata commit after all required remote objects are durable.
- A commit conflict preserves the dirty local cache and never silently overwrites a newer committed version.
- Dirty cache entries are never automatically evicted.
- Raw Linux filename bytes are authoritative; NUL and `/` are rejected; protocol encodes names as unpadded base64url.
- v0.1 excludes hard links, xattrs, POSIX ACLs, device nodes, sockets, named pipes, macOS, Windows, and offline conflict merging.
- License is dual `MIT OR Apache-2.0`.

---

## File Structure

```text
Cargo.toml                         Workspace members, shared dependency versions/lints
rust-toolchain.toml               Stable toolchain declaration
LICENSE-MIT                       MIT license
LICENSE-APACHE                    Apache-2.0 license
README.md                         Build/run/test and architecture quick start
.env.example                      Non-secret server/client configuration names

crates/dcfs-core/
  Cargo.toml
  src/lib.rs                      Re-exports domain modules
  src/ids.rs                      Strong UUID identifiers
  src/name.rs                     Linux filename byte validation
  src/node.rs                     NodeKind, FileAttr, generation metadata
  src/version.rs                  VersionState and version invariants
  src/chunk.rs                    ChunkSpec and fixed-size chunk planner
  tests/domain.rs                 Domain/chunk/name invariant tests

crates/dcfs-objectstore/
  Cargo.toml
  src/lib.rs                      ObjectStore trait and errors
  src/memory.rs                   Deterministic in-memory backend for tests
  tests/object_store.rs           Immutability/get/stat/delete contract

crates/dcfs-crypto/
  Cargo.toml
  src/lib.rs                      Crypto API
  src/chunk.rs                    XChaCha20-Poly1305 + BLAKE3 envelope
  tests/chunk_crypto.rs           Round-trip/tamper/wrong-key tests

crates/dcfs-protocol/
  Cargo.toml
  src/lib.rs                      API version + DTO re-exports
  src/name.rs                     base64url NameBytes wire type
  src/nodes.rs                    metadata request/response DTOs
  src/versions.rs                 staging/chunk/commit DTOs
  src/errors.rs                   stable API error codes
  tests/json_contract.rs          JSON compatibility tests

crates/dcfs-db/
  Cargo.toml
  src/lib.rs                      Repository traits + PgRepository export
  src/model.rs                    DB-facing records
  src/repository.rs               MetadataRepository trait
  src/postgres.rs                 SQLx PostgreSQL implementation
  tests/migration_contract.rs     Migration text/schema invariant tests

migrations/
  0001_initial.sql                nodes, versions, chunks, objects, sessions/jobs

crates/dcfs-server/
  Cargo.toml
  src/lib.rs                      App state/router construction
  src/main.rs                     Server binary
  src/config.rs                   Configuration/secret loading
  src/error.rs                    Domain -> HTTP errors
  src/routes/nodes.rs             Namespace API
  src/routes/versions.rs          Stage/upload/commit/read API
  src/services/commit.rs          Atomic version commit orchestration
  src/services/gc.rs              Reference-driven delayed GC
  src/services/recovery.rs        Staging/upload reconciliation
  src/auth.rs                     Bootstrap credential/session token validation
  src/metrics.rs                  Prometheus metrics
  tests/api_metadata.rs           Router tests with memory repository/store
  tests/api_versions.rs           Atomicity/conflict/upload-failure tests

crates/dcfs-cache/
  Cargo.toml
  src/lib.rs                      Cache public API
  src/dirty.rs                    Dirty extent merge/range logic
  src/entry.rs                    CacheEntry state machine
  src/journal.rs                  Crash-recovery journal
  src/manager.rs                  Clean-entry LRU and byte budget
  tests/cache.rs                  Dirty safety/eviction/recovery tests

crates/dcfs-fuse/
  Cargo.toml
  src/lib.rs                      FUSE adapter exports
  src/main.rs                     Mount binary
  src/client.rs                   Server protocol client
  src/fs.rs                       fuser::Filesystem implementation
  src/handles.rs                  Open handle table/base version capture
  src/errno.rs                    API/domain error -> errno mapping
  tests/semantics.rs              Non-mounted operation semantics using fake client

crates/dcfs-discord/
  Cargo.toml
  src/lib.rs                      DiscordObjectStore
  src/client.rs                   Discord REST primitives
  src/locator.rs                  stable guild/channel/message/attachment locator
  src/ratelimit.rs                bucket/429/backoff state machine
  src/retry.rs                    bounded transient retry policy
  tests/ratelimit.rs              deterministic retry/rate-limit tests
  tests/object_store_mock.rs      mocked HTTP attachment lifecycle

crates/dcfs-cli/
  Cargo.toml
  src/main.rs                     config check, cache recovery, status subcommands

scripts/
  check.sh                        fmt/clippy/test command used locally/CI
  fuse-smoke.sh                   Linux mounted acceptance smoke test

docker/
  docker-compose.yml              PostgreSQL + server development stack

.github/workflows/ci.yml          fmt, clippy, unit/integration tests; optional FUSE job
```

---

### Task 1: Workspace foundation and core domain model

**Files:**
- Create: workspace/toolchain/licenses/README plus `crates/dcfs-core/**`
- Test: `crates/dcfs-core/tests/domain.rs`

**Interfaces:**
- Produces `NodeId`, `FileVersionId`, `ObjectId`, `NodeName`, `NodeKind`, `FileAttr`, `VersionState`, `ChunkSpec`, `plan_chunks(size, chunk_size)`.
- All later crates depend on these exact domain types rather than raw UUID/string substitutes.

- [ ] **Step 1: Write failing domain tests** covering raw non-UTF8 filename acceptance, `/` and NUL rejection, zero/one/multiple chunk boundaries, and version-state terminal rules.

```rust
#[test]
fn rejects_slash_and_nul_names() {
    assert!(NodeName::new(b"a/b".to_vec()).is_err());
    assert!(NodeName::new(b"a\0b".to_vec()).is_err());
}

#[test]
fn plans_unaligned_last_chunk() {
    let chunks = plan_chunks(17, 8).unwrap();
    assert_eq!(chunks.iter().map(|c| c.len).collect::<Vec<_>>(), vec![8, 8, 1]);
}
```

- [ ] **Step 2: Run `cargo test -p dcfs-core` and confirm tests fail** because the types/functions do not exist.
- [ ] **Step 3: Implement the minimal strong-ID, name, node, version, and chunk modules** with checked arithmetic and `chunk_size > 0` validation.
- [ ] **Step 4: Run `cargo fmt --check && cargo test -p dcfs-core && cargo clippy -p dcfs-core --all-targets -- -D warnings` and make them pass.**
- [ ] **Step 5: Commit** `feat(core): add domain model and chunk planner`.

### Task 2: Immutable object-store contract and authenticated chunk crypto

**Files:**
- Create: `crates/dcfs-objectstore/**`, `crates/dcfs-crypto/**`
- Test: contract and crypto tests listed in File Structure.

**Interfaces:**
- Produces async `ObjectStore::{put,get,delete,stat}` where `put(ObjectId, Bytes) -> StoredObject` rejects a conflicting existing object ID.
- Produces `ChunkCrypto::seal(key_id, key, object_id, plaintext) -> EncryptedChunk` and `open(...) -> Vec<u8>`; envelope carries nonce, plaintext hash, ciphertext hash, and key ID.

- [ ] **Step 1: Write failing object-store contract tests** proving put/get/stat, idempotent identical put, conflicting overwrite rejection, and delete/not-found behavior.
- [ ] **Step 2: Write failing crypto tests** for round trip, ciphertext tamper, associated-object-ID mismatch, and wrong key.
- [ ] **Step 3: Run both crate test suites and verify failure.**
- [ ] **Step 4: Implement `MemoryObjectStore` with `RwLock<HashMap<ObjectId, Bytes>>` and immutable semantics.**
- [ ] **Step 5: Implement XChaCha20-Poly1305 with a fresh 24-byte OS-random nonce and BLAKE3 hashes; bind `ObjectId` and key ID as associated data.**
- [ ] **Step 6: Run fmt/test/clippy for both crates and make them pass.**
- [ ] **Step 7: Commit** `feat(storage): add immutable object store and chunk crypto`.

### Task 3: Versioned protocol contract

**Files:**
- Create: `crates/dcfs-protocol/**`
- Test: `crates/dcfs-protocol/tests/json_contract.rs`

**Interfaces:**
- `NameBytes(Vec<u8>)` serializes to unpadded URL-safe base64.
- Every mutation DTO contains `idempotency_key: Uuid`.
- `CommitVersionRequest` contains `expected_generation: u64` and `expected_current_version: Option<FileVersionId>`.
- Stable API error codes include `not_found`, `already_exists`, `invalid_name`, `conflict`, `integrity`, `queue_full`, `unauthorized`, `backend_unavailable`.

- [ ] **Step 1: Write golden JSON tests** for non-UTF8 name round-trip, node-create DTO, stage-version DTO, and commit DTO.
- [ ] **Step 2: Run protocol tests and confirm they fail.**
- [ ] **Step 3: Implement DTOs with serde and explicit API version `v1`.**
- [ ] **Step 4: Run fmt/test/clippy and pass.**
- [ ] **Step 5: Commit** `feat(protocol): define v1 wire contract`.

### Task 4: PostgreSQL schema and repository boundary

**Files:**
- Create: `migrations/0001_initial.sql`, `crates/dcfs-db/**`
- Test: `crates/dcfs-db/tests/migration_contract.rs`

**Interfaces:**
- `MetadataRepository` exposes node lookup/list/create/rename/delete, staged-version creation, chunk attachment, and `commit_version(CommitGuard)`.
- `CommitGuard` includes node ID, staged version ID, expected generation, expected current version.
- `commit_version` is the only API allowed to switch `nodes.current_version_id` and increment generation.

- [ ] **Step 1: Write migration-contract tests** reading `0001_initial.sql` and asserting required tables/constraints/indexes are present, including live-name uniqueness and FK integrity.
- [ ] **Step 2: Run DB tests and verify failure.**
- [ ] **Step 3: Add SQL migration** for `nodes`, `file_versions`, `file_chunks`, `stored_objects`, `upload_jobs`, `sessions`, with check constraints for states/sizes and root handling.
- [ ] **Step 4: Define repository records/trait and implement `PgRepository` with parameterized SQLx queries.**
- [ ] **Step 5: Add a transaction-level commit query that locks the node row, checks generation/current version, commits staging version, supersedes old version, switches pointer, increments generation, and rolls back on conflict.**
- [ ] **Step 6: Run compile-time/unit checks; when `DATABASE_URL` is available also run migration integration tests against PostgreSQL.**
- [ ] **Step 7: Commit** `feat(db): add metadata schema and atomic repository`.

### Task 5: Metadata HTTP API and empty-file vertical slice

**Files:**
- Create: server crate/router/config/error/nodes routes and test-only in-memory repository implementation.
- Test: `crates/dcfs-server/tests/api_metadata.rs`

**Interfaces:**
- Endpoints: resolve/get/children/create/patch/delete/rename plus health/ready.
- Server state owns `Arc<dyn MetadataRepository>` and `Arc<dyn ObjectStore>`.
- API responses use protocol DTOs only.

- [ ] **Step 1: Write router tests** for create nested directory, create empty file, getattr, readdir, rename without object-store calls, unlink, non-empty rmdir rejection, duplicate name, and invalid raw name.
- [ ] **Step 2: Run server metadata tests and verify failure.**
- [ ] **Step 3: Implement a deterministic `MemoryMetadataRepository` for server/integration tests.**
- [ ] **Step 4: Implement Axum routes and domain-to-HTTP mapping with request IDs.**
- [ ] **Step 5: Run server metadata tests plus workspace fmt/clippy.**
- [ ] **Step 6: Commit** `feat(server): add metadata API vertical slice`.

### Task 6: Immutable version staging and atomic commit service

**Files:**
- Create/modify: `dcfs-server/src/routes/versions.rs`, `services/commit.rs`, repository memory/Pg methods
- Test: `crates/dcfs-server/tests/api_versions.rs`

**Interfaces:**
- Stage returns a `FileVersionId` tied to node/base generation.
- Chunk upload receives plaintext, verifies requested logical chunk metadata, hashes/encrypts, writes immutable object, then attaches object metadata to staging manifest.
- Commit verifies full contiguous manifest and total size before repository atomic switch.

- [ ] **Step 1: Write failing tests** for multi-chunk file round trip, failed object-store put preserving old version, missing chunk preventing commit, and two writers where second commit returns conflict and first committed bytes remain visible.
- [ ] **Step 2: Run version tests and verify failure.**
- [ ] **Step 3: Implement staging and chunk upload using `ChunkCrypto` + `ObjectStore`.**
- [ ] **Step 4: Implement manifest completeness/integrity validation and atomic `commit_version`.**
- [ ] **Step 5: Implement chunk read that fetches ciphertext and verifies/decrypts before responding.**
- [ ] **Step 6: Run server version tests and entire workspace tests.**
- [ ] **Step 7: Commit** `feat(server): add transactional encrypted file versions`.

### Task 7: Local write-back cache, dirty extents, and crash journal

**Files:**
- Create: `crates/dcfs-cache/**`
- Test: `crates/dcfs-cache/tests/cache.rs`

**Interfaces:**
- `DirtyExtents::insert(Range<u64>)` merges overlapping/adjacent ranges.
- `CacheEntry` tracks node/base-version/generation/logical-size/state and local path.
- `CacheManager` may evict only `Clean` entries and preserves all dirty/conflicted entries.
- Journal uses atomic temp-write + rename records sufficient to discover recoverable dirty entries after restart.

- [ ] **Step 1: Write failing extent tests** for overlap, adjacency, sparse ranges, truncate grow/shrink marking.
- [ ] **Step 2: Write failing cache tests** showing dirty entries survive budget pressure while LRU clean entries are removed.
- [ ] **Step 3: Write failing journal recovery test** simulating process restart from persisted record.
- [ ] **Step 4: Implement dirty map, state machine, LRU manager, and atomic journal.**
- [ ] **Step 5: Run cache tests under temp directories and pass fmt/clippy.**
- [ ] **Step 6: Commit** `feat(cache): add durable write-back cache state`.

### Task 8: FUSE client semantics and Linux mount

**Files:**
- Create: `crates/dcfs-fuse/**`, `scripts/fuse-smoke.sh`
- Test: `crates/dcfs-fuse/tests/semantics.rs`

**Interfaces:**
- `ServerClient` trait abstracts protocol operations so semantics tests do not need a mounted kernel filesystem.
- Open-write captures base version/generation once.
- `write` and `truncate` modify local cache only.
- `fsync` freezes a snapshot and executes stage/upload/commit; conflict leaves entry `Conflicted` and journaled.
- `rename` is metadata-only.

- [ ] **Step 1: Write fake-client semantics tests** for create/write/read, unaligned overwrite preserving surrounding bytes, truncate shrink/grow, fsync conflict preserving dirty cache, and rename causing zero chunk uploads.
- [ ] **Step 2: Run semantics tests and verify failure.**
- [ ] **Step 3: Implement HTTP `ServerClient` and handle table.**
- [ ] **Step 4: Implement cache-backed filesystem service independent of fuser callbacks, then make semantics tests pass.**
- [ ] **Step 5: Map service to `fuser::Filesystem` operations required by spec and explicit errno mapping.**
- [ ] **Step 6: On Linux with `/dev/fuse`, run `scripts/fuse-smoke.sh` with `cp`, `mv`, `rm`, `mkdir/rmdir`, `truncate`, `dd ... seek=`, checksum, and small-tree rsync. If FUSE is unavailable, keep the kernel smoke test skipped with a clear prerequisite while semantics tests remain mandatory.**
- [ ] **Step 7: Commit** `feat(fuse): add full read-write writeback mount`.

### Task 9: Discord ObjectStore with compliant rate limiting

**Files:**
- Create: `crates/dcfs-discord/**`
- Test: `ratelimit.rs`, `object_store_mock.rs`

**Interfaces:**
- Durable locator is `{guild_id, channel_id, message_id, attachment_id}`; signed CDN URL is cache-only.
- Request scheduler uses a semaphore for global concurrency and per-bucket deadline state.
- A 429 records the exact server-directed delay; transient network/5xx uses capped exponential backoff with jitter.
- `put` never retries blindly after an ambiguous successful upload; it reconciles the idempotency record/message state first.

- [ ] **Step 1: Write deterministic rate-limit tests with paused Tokio time** proving 429 waits, no early retry, bounded concurrency, 5xx backoff, and non-retryable 4xx behavior.
- [ ] **Step 2: Write mocked HTTP lifecycle tests** for upload, fetch/refetch expired attachment metadata, get bytes, stat, and delete.
- [ ] **Step 3: Run Discord crate tests and verify failure.**
- [ ] **Step 4: Implement REST client, locator, scheduler, retry policy, and `ObjectStore` adapter.**
- [ ] **Step 5: Run tests/clippy; ensure logs redact Authorization headers/token values.**
- [ ] **Step 6: Commit** `feat(discord): add rate-limit-aware object store`.

### Task 10: Recovery, GC, auth, observability, packaging, and acceptance suite

**Files:**
- Create/modify: server recovery/GC/auth/metrics, CLI, Docker Compose, CI, README, `.env.example`, acceptance scripts.
- Test: server recovery/GC/auth tests and optional Discord smoke test.

**Interfaces:**
- Session token is short-lived and namespace scoped; bootstrap secret is server-side config.
- GC selects only objects with zero references from committed/staging versions older than grace period.
- Recovery resumes or expires stale staged upload jobs without changing current committed version.
- `/metrics` exports Discord request/429/wait, bytes, queue depth, staged/orphan counts, commit latency, integrity failures.

- [ ] **Step 1: Write failing recovery/GC tests** for orphan upload after simulated DB failure, stale staging expiration, and referenced-object protection.
- [ ] **Step 2: Write failing auth tests** for missing/invalid/expired session and namespace scope.
- [ ] **Step 3: Implement recovery + reference-driven delayed GC and idempotent deletion.**
- [ ] **Step 4: Implement bootstrap/session auth, secret redaction, health/readiness, and Prometheus metrics.**
- [ ] **Step 5: Add CLI `config-check`, `cache-recover`, and `status`; Docker PostgreSQL/server stack; CI fmt/clippy/test jobs.**
- [ ] **Step 6: Run `scripts/check.sh`; on an eligible Linux runner run FUSE acceptance suite.**
- [ ] **Step 7: If explicit Discord test secrets are present, run opt-in smoke test proving upload/read/delete and mocked/real 429-compliant behavior; never make this normal CI.**
- [ ] **Step 8: Update README with security/compliance limitations and recovery instructions.**
- [ ] **Step 9: Commit** `feat: complete DCFS v0.1 recovery and operations`.

---

## Final Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] PostgreSQL migrations apply cleanly to an empty database and repository integration tests pass.
- [ ] FUSE Linux acceptance: nested dirs, >1-chunk copy, unmount/remount checksum, unaligned random write, truncate shrink/grow, metadata-only rename, unlink-before-GC, upload-failure atomicity, client/server restart, tamper detection.
- [ ] Discord adapter test demonstrates exact 429 backoff behavior and bounded concurrency.
- [ ] `git status --short` is clean and all implementation commits are present.
