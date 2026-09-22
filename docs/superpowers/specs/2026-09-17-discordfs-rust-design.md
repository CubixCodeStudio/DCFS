# DiscordFS Rust Design

Date: 2026-09-17
Status: Proposed v0.1 design, approved in chat pending written-spec review

## 1. Purpose

DiscordFS is a clean-room Rust implementation of a Linux FUSE filesystem that presents Discord-hosted attachments as a POSIX-like mounted filesystem while keeping authoritative filesystem metadata in PostgreSQL and mutable working state in a local cache.

The project is not a fork and will not copy source code from DISFS. The initial target is Linux with FUSE3. macOS and additional object-storage backends are explicitly deferred.

## 2. Goals

The v0.1 system SHALL:

- Mount a filesystem through FUSE3 on Linux.
- Support ordinary file operations: create, open, read, write, truncate, fsync, flush, release, rename, unlink, mkdir, rmdir, getattr, setattr and readdir.
- Support random writes by using a local write-back cache rather than attempting in-place mutation of Discord attachments.
- Store remote file data as immutable encrypted chunks.
- Commit a new file version atomically only after all required chunks are durable remotely.
- Preserve the last committed version if an upload fails or the client/server crashes mid-commit.
- Keep path/inode metadata in PostgreSQL instead of encoding the filesystem tree in Discord messages.
- Respect Discord API rate limits and retry guidance; never attempt to bypass or distribute requests to evade limits.
- Make chunk size configurable and validate it against the active Discord upload constraints.
- Provide integrity checking for every chunk and every committed file version.
- Keep the Discord storage implementation behind an object-store interface so another backend can be added later without changing FUSE semantics.

## 3. Non-goals for v0.1

The first release will not promise:

- Strict NFS-style multi-writer coherency across multiple mounted clients.
- mmap writeback semantics beyond what the selected FUSE library can safely support.
- POSIX ACLs, xattrs, hard links, device nodes, sockets or named pipes.
- macOS support.
- Windows support.
- Transparent deduplication across users.
- Offline conflict merging.
- High-availability PostgreSQL orchestration.
- Unlimited storage or any behavior intended to circumvent Discord platform limits.

## 4. Compliance boundary

Discord's API documentation states that HTTP clients must honor rate limiting and that repeatedly ignoring limits can lead to API-key revocation. Discord's Developer Terms also prohibit excessive or abusive API usage and attempts to exceed usage limits.

Therefore DiscordFS SHALL treat Discord storage as a constrained backend. It SHALL:

- obey 429 responses and Retry-After / rate-limit headers;
- avoid token rotation, bot rotation or channel sharding for the purpose of evading rate limits;
- use bounded concurrency;
- expose queue depth and throttling metrics;
- fail closed rather than bypass Discord limits;
- keep the storage abstraction replaceable so deployments that need high-throughput object storage can move to S3-compatible storage.

Discord documentation currently reports a default API upload limit of 10 MiB per file, while consumer account limits may differ. Because API limits can change, DiscordFS SHALL NOT bake a fixed Discord maximum into filesystem metadata. The safe initial default chunk payload is 8 MiB, configurable by the server.

## 5. Architecture

The system is split into a Rust workspace with independent crates:

```text
discordfs-rs/
├── Cargo.toml
├── crates/
│   ├── discordfs-core/
│   ├── discordfs-protocol/
│   ├── discordfs-fuse/
│   ├── discordfs-server/
│   ├── discordfs-db/
│   ├── discordfs-objectstore/
│   ├── discordfs-discord/
│   ├── discordfs-crypto/
│   └── discordfs-cli/
├── migrations/
├── tests/
├── docker/
└── docs/
```

### 5.1 `discordfs-core`

Owns domain types and invariants only. It has no HTTP, SQL, FUSE or Discord dependencies.

Key concepts:

- `NodeId`: stable filesystem identity.
- `NodeKind`: file or directory.
- `FileVersionId`: immutable committed version.
- `ChunkId`: logical chunk identity within a version.
- `ObjectId`: remote immutable encrypted object.
- `FileAttr`: portable subset of POSIX attributes.
- `VersionState`: staging, committed, superseded, garbage.

### 5.2 `discordfs-protocol`

Defines versioned client/server request and response schemas. The transport for v0.1 is HTTP/JSON for metadata/control plus streaming HTTP bodies for chunk transfer. The protocol crate contains DTOs but no server implementation.

Every mutating operation carries an idempotency key. Commit endpoints use an expected-current-version field for optimistic concurrency control.

### 5.3 `discordfs-fuse`

Linux FUSE3 client. Responsibilities:

- map inode operations to server metadata operations;
- maintain open-handle state;
- manage local cache files;
- maintain dirty-range tracking;
- assemble reads from local cache or downloaded chunks;
- implement write-back and commit on fsync/release according to configured policy;
- map server errors to errno values.

The FUSE layer never calls Discord directly.

### 5.4 `discordfs-server`

Stateless API service except for bounded worker queues. Responsibilities:

- authentication and session authorization;
- metadata transactions;
- version staging/commit;
- upload/download orchestration;
- garbage-collection scheduling;
- object-store abstraction;
- rate-limit aware background workers;
- health and metrics endpoints.

### 5.5 `discordfs-db`

PostgreSQL implementation of metadata repositories and migrations. SQL is isolated behind repository traits used by the server.

### 5.6 `discordfs-objectstore`

Defines the storage abstraction:

```text
ObjectStore
  put(object_id, bytes) -> StoredObject
  get(locator) -> byte stream
  delete(locator)
  stat(locator)
```

The interface treats objects as immutable. No overwrite operation exists.

### 5.7 `discordfs-discord`

Discord implementation of `ObjectStore`.

It owns:

- REST requests for creating/fetching/deleting attachment-bearing messages;
- parsing/storing stable Discord identifiers;
- refreshing attachment download URLs by refetching message metadata when signed URLs expire;
- Discord rate-limit buckets, 429 handling and bounded retries;
- bounded upload/download concurrency.

Database metadata SHALL store guild/channel/message/attachment identifiers as the durable locator, not rely solely on a signed CDN URL because Discord documents attachment CDN URLs as expiring.

### 5.8 `discordfs-crypto`

Provides chunk encryption and integrity operations.

Initial design:

- BLAKE3 for plaintext chunk hashing and whole-file content hashing.
- XChaCha20-Poly1305 AEAD for chunk encryption.
- fresh random nonce per encrypted object.
- key IDs in metadata; raw encryption keys are never stored in Discord.
- v0.1 server-side master key supplied from an environment secret or mounted secret file.

Key rotation is designed into metadata but an automated rotation workflow is deferred.

## 6. Filesystem model

Nodes form a tree:

```text
nodes
- id UUID primary key
- parent_id UUID null for root
- name bytea containing the raw Linux filename bytes (NUL and `/` are rejected)
- kind
- mode
- uid
- gid
- size
- atime
- mtime
- ctime
- current_version_id UUID nullable
- generation bigint
- deleted_at nullable
```

A unique constraint protects `(parent_id, name)` for live nodes.

Directories have no file version. Regular files point at the latest committed immutable file version.

Paths are resolved to stable node IDs. Rename changes metadata only and does not rewrite remote chunks.

## 7. File version model

```text
file_versions
- id UUID primary key
- node_id UUID
- base_version_id UUID nullable
- state
- size
- plaintext_hash
- chunk_size
- created_at
- committed_at nullable
```

```text
file_chunks
- version_id UUID
- chunk_index bigint
- logical_offset bigint
- plaintext_size integer
- plaintext_hash
- object_id UUID
- primary key(version_id, chunk_index)
```

```text
discord_objects
- id UUID primary key
- guild_id bigint/text
- channel_id bigint/text
- message_id bigint/text
- attachment_id bigint/text
- ciphertext_size bigint
- crypto_key_id
- nonce
- object_hash
- created_at
- deleted_at nullable
```

Staging versions are never visible to readers through `nodes.current_version_id`.

## 8. Write path

### 8.1 Open for write

1. Resolve path to node and current version.
2. Create/open a local cache entry.
3. Materialize required data lazily or fully depending on operation.
4. Capture the base version and node generation.
5. Return a FUSE file handle pointing to local state.

### 8.2 Write

`write(offset, bytes)` writes only to the local cache file and records dirty extents. The server is not called for every small kernel write.

A truncate updates the local cache representation and dirty state.

### 8.3 Commit

On fsync, explicit sync policy, or final release:

1. freeze a snapshot of the local cache entry;
2. split the logical file into configured fixed-size chunks;
3. hash chunks;
4. compare against the base version and reuse unchanged remote objects where safe;
5. encrypt changed/new chunks;
6. upload new ciphertext objects;
7. create a staging version and chunk manifest;
8. verify every object needed by the version exists;
9. in one PostgreSQL transaction, compare the expected node generation/current version and switch `current_version_id` to the new committed version;
10. mark the previous version superseded;
11. enqueue unreferenced objects for delayed garbage collection.

If any step before transaction commit fails, the previous version remains visible.

### 8.4 Concurrent writers

v0.1 uses optimistic concurrency. If another writer commits after this handle's base version, the commit fails with a conflict rather than silently overwriting remote changes. The FUSE client maps this to a visible I/O/conflict error and preserves the dirty local cache for recovery.

## 9. Read path

1. Resolve node and committed version.
2. Check local cache for requested range.
3. Determine required chunk indexes.
4. Fetch missing remote objects through the server.
5. decrypt and authenticate each chunk;
6. verify plaintext hash;
7. populate cache;
8. satisfy the requested FUSE read range.

Read-ahead may be added with a bounded window. Initial correctness does not depend on read-ahead.

## 10. Local cache

The cache is authoritative only for uncommitted local modifications. Remote committed metadata remains authoritative for shared state.

Cache entries contain:

- node ID;
- base version ID;
- generation;
- local file path;
- dirty range map;
- logical size;
- last-access time;
- dirty/clean/committing/conflicted state.

Dirty entries SHALL never be evicted automatically.

Clean entries are evicted by an LRU policy using a configurable byte budget.

A journal records dirty entries so the client can discover recoverable writes after an unclean shutdown.

## 11. Metadata operations

- `mkdir`: metadata-only transaction.
- `rmdir`: succeeds only for an empty directory.
- `rename`: metadata-only transaction with normal replace/no-replace rules supported by the implementation.
- `unlink`: removes the namespace reference immediately; remote objects become GC candidates only after no committed/staging version references them.
- `getattr`: served from metadata, not Discord.
- `readdir`: served from metadata, paginated by the server internally.
- `chmod/chown/utimens`: metadata-only where permitted.

Hard links are deferred so a node has exactly one parent in v0.1.

## 12. API surface

Illustrative v1 endpoints:

```text
POST   /v1/session
GET    /v1/nodes/resolve
GET    /v1/nodes/{id}
GET    /v1/nodes/{id}/children
POST   /v1/nodes
PATCH  /v1/nodes/{id}
DELETE /v1/nodes/{id}
POST   /v1/nodes/{id}/rename

POST   /v1/files/{id}/versions/stage
PUT    /v1/versions/{id}/chunks/{index}
POST   /v1/versions/{id}/commit
DELETE /v1/versions/{id}
GET    /v1/versions/{id}/chunks/{index}

GET    /healthz
GET    /readyz
GET    /metrics
```

This list is intentionally small. Exact schemas are defined in the protocol crate and tested as a compatibility surface. Because Linux filenames are arbitrary bytes rather than guaranteed UTF-8, protocol fields representing a single filename encode the raw bytes as unpadded base64url; display-only paths may use lossy UTF-8 rendering but are never authoritative identifiers.

## 13. Authentication

v0.1 assumes a trusted self-hosted deployment but still uses authenticated clients.

- Server receives a pre-shared client credential from a secret store/environment for initial bootstrap.
- Session tokens are short-lived and scoped to a filesystem namespace/user.
- Discord bot token remains server-side only.
- FUSE clients never receive the Discord token or database credentials.
- TLS termination is required for non-loopback/non-private deployments.

A richer OIDC/device-flow login is deferred.

## 14. Discord storage strategy

Each encrypted chunk is uploaded as an attachment in a bot-authored message in configured storage channels.

The backend stores Discord identifiers needed to refetch the message and derive a currently valid attachment URL.

The server chooses storage channels from configuration for organization and capacity management, but SHALL NOT distribute traffic across channels/bots to evade Discord rate limits.

Deletion is asynchronous. Removing a file from the filesystem does not synchronously delete Discord messages because old versions, retries or other references may still need them.

## 15. Rate limiting and retry

The Discord adapter maintains per-route/per-bucket state based on Discord response headers.

Rules:

- Respect Retry-After exactly for 429 responses.
- Bound total concurrent Discord requests.
- Exponential backoff with jitter for transient 5xx/network errors.
- Do not blindly retry non-idempotent operations; uploads use an internal idempotency record and reconcile response state before retrying.
- Surface queue saturation to callers rather than spawning unbounded tasks.
- Persist enough upload-job state for recovery after server restart.

## 16. Garbage collection

GC is reference-driven and delayed.

1. A version becomes superseded or a node is unlinked.
2. After a retention grace period, GC finds remote objects referenced by no live committed/staging version.
3. The worker deletes the Discord message/object.
4. Metadata is marked deleted only after Discord confirms deletion or the object is proven absent.

A configurable retention period allows recovery from accidental deletion and avoids races with transactions.

## 17. Failure handling

### Client crash with dirty file

Dirty cache journal remains. Next mount reports recoverable dirty entries. No uncommitted data is advertised as remote state.

### Server crash during upload

Staging version and upload jobs remain incomplete. Recovery workers resume or expire them. `current_version_id` is unchanged.

### Discord upload failure

Commit does not occur. Existing version remains readable.

### PostgreSQL failure after uploads

Uploaded objects are orphan candidates. Reconciliation/GC removes them after the grace period.

### Signed CDN URL expiry

Server refetches the Discord message using stored identifiers and obtains a fresh attachment object/URL.

### Corrupt or unauthentic chunk

AEAD/hash verification fails; the chunk is rejected and the read returns an integrity I/O error. The corrupted bytes are never returned to the filesystem caller.

## 18. Observability

The server exports metrics for:

- Discord request rate;
- rate-limit waits and 429 count;
- upload/download bytes;
- upload queue depth;
- staging version count;
- orphan object count;
- commit latency;
- cache hit ratio as reported by clients when telemetry is enabled;
- integrity failures.

Structured logs include request IDs, node/version IDs and job IDs but never encryption keys, tokens or plaintext file contents.

## 19. Testing strategy

### Unit tests

- path/name validation;
- chunk planning;
- dirty extent behavior;
- version state transitions;
- crypto round trips and tamper detection;
- retry/rate-limit state machine;
- repository invariants.

### Integration tests with fake object store

A deterministic in-memory/local object store is the default integration backend so tests do not depend on Discord.

Required scenarios:

- create/read/write/unlink;
- overwrite existing bytes;
- unaligned random writes;
- grow/shrink truncate;
- rename files/directories;
- fsync atomicity;
- failed chunk upload leaves old version visible;
- server restart during staged upload;
- concurrent writer conflict;
- cache eviction never removes dirty data;
- crash recovery journal.

### FUSE end-to-end tests

On Linux CI or a privileged test runner:

```text
cp
mv
rm
mkdir/rmdir
truncate
dd with seek
sha256sum comparison
rsync small tree
parallel readers
```

Discord-backed tests are opt-in smoke tests requiring explicit secrets and a dedicated test guild/channel. They are never part of ordinary pull-request CI.

## 20. Security requirements

- No Discord token on FUSE clients.
- No raw master key in PostgreSQL or Discord.
- AEAD authenticated encryption per remote object.
- Constant-time cryptographic implementations supplied by established Rust crates; no custom crypto primitives.
- SQL queries parameterized through the DB layer.
- Strict maximum sizes for API payloads and metadata fields.
- Path traversal has no meaning at the server API because operations use node IDs after resolution.
- Downloaded bytes are verified before becoming readable cached plaintext.
- Secrets are redacted from logs.

## 21. Initial technology choices

Subject to implementation-plan verification:

- Rust stable edition supported by the chosen crates.
- FUSE: `fuser`/FUSE3 on Linux.
- Async runtime: Tokio.
- Server: Axum.
- HTTP client: reqwest, with Discord-specific rate-limit and retry behavior implemented inside `discordfs-discord` behind the `ObjectStore` trait.
- PostgreSQL: SQLx.
- Serialization: serde.
- Crypto: established RustCrypto-compatible crates for XChaCha20-Poly1305; BLAKE3.
- CLI/config: clap + serde-based TOML/environment configuration.
- Observability: tracing + metrics/Prometheus exporter.

Versions will be pinned in the implementation plan after checking current compatible releases.

## 22. Delivery sequence

The implementation should proceed vertically rather than building every subsystem in isolation:

1. Core domain model + fake object store + PostgreSQL migrations.
2. Metadata API with directory and empty-file semantics.
3. Minimal FUSE mount supporting getattr/readdir/create/read empty files.
4. Local cache and write/truncate path using fake object store.
5. Immutable chunk/version commit path and atomic recovery tests.
6. Encryption layer.
7. Discord object-store adapter with strict rate-limit handling.
8. Garbage collection and crash reconciliation.
9. Authentication hardening and observability.
10. Discord smoke test and packaging.

Each stage must leave the test suite green before proceeding.

## 23. Acceptance criteria for v0.1

On a Linux test host, after mounting `/mnt/discordfs`, the following must complete correctly against the server:

- create nested directories;
- copy a file larger than one remote chunk;
- verify exact bytes after unmount/remount;
- perform an unaligned random write and verify unchanged surrounding bytes;
- truncate a file smaller and larger;
- rename a large file without re-uploading its data;
- unlink a file and confirm namespace removal before asynchronous GC;
- survive a forced upload failure without losing the last committed file version;
- survive client/server restart with committed files intact;
- detect intentionally corrupted ciphertext;
- demonstrate that a Discord 429 causes compliant backoff rather than limit evasion.

## 24. Repository and licensing

This is a new clean-room repository. No source code from DISFS is copied.

Project license: dual MIT OR Apache-2.0.


## 25. External references

- Discord API Reference: https://docs.discord.com/developers/reference
- Discord Developer Terms of Service: https://support-dev.discord.com/hc/en-us/articles/8562894815383-Discord-Developer-Terms-of-Service
- Discord Developer Policy: https://support-dev.discord.com/hc/en-us/articles/8563934450327-Discord-Developer-Policy

These are runtime/platform constraints, not implementation dependencies. DiscordFS must be revalidated against current Discord documentation before a public release because upload and platform limits may change.
