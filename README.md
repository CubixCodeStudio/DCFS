# DCFS

A Linux FUSE filesystem that stores file contents as encrypted chunks and keeps its
namespace in PostgreSQL. Files are split into chunks, sealed with XChaCha20-Poly1305
before they leave the server, and made visible through one atomic version commit.

> **Status: v0.1, work in progress.** The filesystem mounts and works: `cp`, `mv`, `rm`,
> `mkdir`/`rmdir`, unaligned overwrites and multi-chunk files all round-trip, and data
> survives an unmount/remount (`scripts/fuse-smoke.sh` checks exactly this on every CI
> run). Chunks currently land in a **local directory**, not in Discord —
> see [Current limitations](#current-limitations).

## Architecture

```
  dcfs-fuse  ──HTTP──▶  dcfs-server  ──▶  dcfs-db (PostgreSQL)
  (mount, buffers)           (chunking, crypto)  └─▶  dcfs-objectstore
                                                       (local dir today,
                                                        Discord attachments later)
```

The client sends plain bytes and never sees a chunk, an object id or a key: chunking,
encryption and the version commit all happen server-side, so the master key and the
database credentials stay on the server.

| Crate | Role |
|---|---|
| `dcfs-core` | Domain model: nodes, versions, chunk planning, ids, names, errors. No I/O. |
| `dcfs-protocol` | Wire types shared by client and server. |
| `dcfs-server` | Axum HTTP API: namespace, byte I/O, versions, objects, health. |
| `dcfs-db` | Metadata repository: `PgRepository` (SQLx) plus an in-memory impl for tests. |
| `dcfs-objectstore` | Immutable object storage: `FsObjectStore` (local dir) and an in-memory impl. |
| `dcfs-crypto` | Per-chunk XChaCha20-Poly1305 sealing with BLAKE3 hashes. |
| `dcfs-fuse` | `Fs` service (inode map, write buffering) plus the `fuser` adapter and mount binary. |
| `dcfs-discord` | Discord API client: upload/download, rate limiting, retries, expiring-URL refresh. |
| `dcfs-cache` | Dirty extents, LRU accounting and the crash journal the FUSE write log is built on. |
| `dcfs-cli` | `config-check` and `status` for an operator. |

## Features

- POSIX file operations: create, read, write, rename, unlink, mkdir, rmdir, truncate, symlink
- `rename(2)` replaces the destination, so save-to-temp-then-rename works (git, editors)
- Chunk-granular writes: an edit re-uploads only the chunks it touches
- Client-side write coalescing on part boundaries, so a `cp` is one request per part
  rather than one per 4 KiB, and the server replaces whole parts instead of merging
- `stream` and `mirror` mount modes, with a local block cache keyed by committed version
- Immutable encrypted chunks (XChaCha20-Poly1305, BLAKE3 hashes, key bound as AAD)
- Atomic version commits with optimistic concurrency control
- Garbage collection: deleting or overwriting a file removes its bytes from the backend
- Range reads that fetch only the parts they cover, however large the file
- Raw Linux filename bytes preserved end to end, including non-UTF-8 names
- Bearer-token authentication on every API route

## Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- PostgreSQL 14+
- Linux with FUSE3 (`libfuse3-dev` to build, `fuse3` to mount). Building the server alone
  works anywhere; mounting is Linux-only in v0.1.

## Installation

### System Dependencies

**Ubuntu/Debian:**
```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libfuse3-dev fuse3 postgresql
```

**Fedora/RHEL:**
```bash
sudo dnf install -y gcc pkgconfig fuse3-devel fuse3 postgresql-server
```

**Arch Linux:**
```bash
sudo pacman -S base-devel fuse3 postgresql
```

**macOS (server only, no FUSE mount):**
```bash
brew install postgresql
```

### Build from Source

```bash
git clone https://github.com/yourusername/dcfs-rs.git
cd dcfs-rs
cargo build --release
```

Binaries will be in `target/release/`:
- `dcfs-server` - HTTP API server
- `dcfs-fuse` - FUSE client (Linux only)
- `dcfs-cli` - Operator CLI: `config-check`, `status`, and `put` (upload a
  file, resumable)

### Install Binaries

```bash
sudo cp target/release/dcfs-server /usr/local/bin/
sudo cp target/release/dcfs-fuse /usr/local/bin/
sudo cp target/release/dcfs-cli /usr/local/bin/
```

## Quick start

Two commands, and nothing to install but Docker:

```bash
cd docker
{ echo "MASTER_KEY=$(openssl rand -hex 32)"
  echo "API_TOKEN=$(openssl rand -hex 32)"; } > .env
docker compose up
```

That builds the server, starts PostgreSQL, applies every migration, and serves
on `http://localhost:8080`. Chunks go to a Docker volume, so it runs without
Discord credentials; fill in the webhook lines in `docker/.env` to store them as
attachments instead.

**Keep that `.env`.** `MASTER_KEY` is the only thing that can decrypt what has
been written and v0.1 has no key rotation, so a new one makes every existing
file unreadable. It is gitignored, which is not the same as backed up.

```bash
. docker/.env
curl -H "authorization: Bearer $API_TOKEN" localhost:8080/api/v1/fs
docker compose -f docker/docker-compose.yml exec server dcfs config-check
```

Stop with `docker compose down`, or `down -v` to throw the data away too.

### Without Docker

```bash
docker compose -f docker/docker-compose.yml up -d postgres

export DATABASE_URL="postgresql://postgres:dev@localhost:55432/dcfs"
export DATABASE_AUTO_MIGRATE=true
export OBJECT_STORE_PATH="$PWD/data/objects"
export MASTER_KEY="$(openssl rand -hex 32)"   # keep this: losing it loses every file
export API_TOKEN="$(openssl rand -hex 32)"
cargo run --release --bin dcfs-server
```

### Over a network

The client and the server are separate processes speaking HTTP, so a mount does
not have to be on the same machine as the server — that part is already true.
What is missing over a real network is TLS: the bearer token is in the request,
and without it anything on the path can read it.

```bash
cd docker
echo "DCFS_DOMAIN=dcfs.example.org" >> .env      # or leave it as localhost
docker compose --profile tls up -d
```

That puts Caddy in front on port 443. By default it issues a certificate from
its own local CA, which is enough for a LAN as long as every client trusts that
CA; point `DCFS_DOMAIN` at a name that resolves publicly and drop the
`tls internal` line in `docker/Caddyfile` to have a public certificate fetched
instead. The client needs no change beyond the URL, as it takes any:

```bash
DCFS_SERVER=https://dcfs.example.org DCFS_TOKEN="$API_TOKEN" dcfs-fuse /mnt/dcfs
```

**One token is not a user system.** Everyone who can reach the server holds the
same secret and every file is reachable with it — `POST /api/v1/sessions` issues
revocable tokens with an expiry, but each one still opens everything. There is
one namespace, `uid` and `gid` are stored but nothing is authorised against
them. Do not put a shared filesystem in front of people who should not all see
each other's files.

### Mounting it

The FUSE client is not in the compose stack: it needs `/dev/fuse` and a mount
point on the host, so it runs on the host and points at whichever server it
should talk to. Linux only.

```bash
mkdir -p /tmp/dcfs
DCFS_SERVER=http://localhost:8080 DCFS_TOKEN="$API_TOKEN" \
  cargo run --release --bin dcfs-fuse -- /tmp/dcfs
```

Unmount with `fusermount3 -u /tmp/dcfs`.

## Deploying

The server is one binary with no state of its own: everything durable is in
PostgreSQL and in the backend. That is what makes the rest of this simple.

**What it needs.** PostgreSQL 14+, a `MASTER_KEY` and an `API_TOKEN`, and
somewhere to put bytes — `OBJECT_STORE_PATH` for a local directory, or
`DISCORD_WEBHOOK_ID`/`DISCORD_WEBHOOK_TOKEN` (and `DISCORD_WEBHOOKS` for more
than one). `dcfs-cli config-check` reads the environment the server would
read and says what it would do, without starting it or touching the database.

**Secrets come from the environment, never from a file in the repo.** Losing
`MASTER_KEY` loses every file: there is no key rotation in v0.1 and no way to
read a chunk without it. Back it up somewhere that is not the same disk as the
database.

**Run it under systemd**, or anything that keeps a process alive and passes it
an environment:

```ini
[Unit]
Description=DCFS server
After=network-online.target postgresql.service

[Service]
ExecStart=/usr/local/bin/dcfs-server
EnvironmentFile=/etc/dcfs/env      # chmod 600, owned by the service user
User=dcfs
Restart=on-failure
RestartSec=5
# It needs no filesystem of its own beyond the object store path.
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/dcfs

[Install]
WantedBy=multi-user.target
```

**More than one instance** can share a database; see the section above on what
holds across them. Size `DB_MAX_CONNECTIONS` above the number of writers you
expect to overlap: a writer holds a pooled connection for the length of its
write, uploads included, which over Discord is seconds per part.

**Migrations are opt-in.** `DATABASE_AUTO_MIGRATE=true` applies every one of
them in order at startup; leave it off and apply `migrations/*.sql` yourself,
in name order, before the new binary starts. They are additive, so an older
server keeps running against a newer schema.

**Mounting is Linux-only.** `dcfs-fuse` needs `fuse3` and a user allowed to
mount; the server itself builds and runs anywhere. A mount and the server it
talks to do not have to be on the same machine — point `DCFS_SERVER` at it
and give the mount `DCFS_TOKEN`.

**Back up the database, not just the bytes.** Objects without their metadata are
unreadable: the manifest is what says which parts make up which file and in what
order.

## Modes: stream and mirror

The mount picks how much it keeps locally, the same choice Google Drive offers:

```bash
dcfs-fuse /mnt/dcfs --mode stream --cache-size 2147483648   # default
dcfs-fuse /mnt/dcfs --mode mirror
```

| | `stream` (default) | `mirror` |
|---|---|---|
| Local disk | Bounded by `--cache-size`, 1 GiB by default | Room for the whole filesystem |
| First read of a block | Fetched from the server | Usually already local |
| Cache policy | Least-recently-used blocks are evicted | Nothing is evicted |
| At mount | Nothing is fetched | The whole tree is pulled down in the background |
| Offline reads | Only what is still cached | Everything already mirrored |

Both modes are the same code path: reads go through a local cache of 1 MiB
blocks keyed by `(file, committed version)`, so a new version misses the cache
rather than serving stale bytes, and a write drops the file's blocks. Mirror
mode is stream mode that never evicts, plus a background walk that reads every
file once. The mount is usable immediately in either mode — anything not
mirrored yet is fetched on demand.

`--cache-dir` chooses where blocks live (default a per-process directory under
`$TMPDIR`, removed on unmount). **Cached blocks are plaintext** — decryption
happens on the server — so put the cache directory somewhere only you can read.

## Configuration

Copy `.env.example` to `.env` and fill it in. The server reads the process environment —
there is no `.env` parsing — so export the variables yourself:

```bash
set -a; . ./.env; set +a
```

The server refuses to start without `MASTER_KEY`, `API_TOKEN` and `OBJECT_STORE_PATH`:
each of the three has a silent-data-loss or silent-exposure failure mode, so none of them
gets a default. `.env.example` documents every variable, including which ones are parsed
but not yet used.

**Never commit a filled-in `.env`.** If a token, key or connection string reaches a
commit, a log or a screenshot, treat it as leaked and rotate it.

## Using a database you already have

DCFS does not need a database of its own. Point `DATABASE_URL` at an
existing one and give it a schema:

```bash
export DATABASE_URL="postgresql://user:pass@db.internal/appdb"
export DATABASE_SCHEMA=dcfs     # default is public
export DATABASE_AUTO_MIGRATE=true    # or apply the SQL yourself, see below
```

What that does and does not do:

- It **never creates a database**, and never touches one you do not name.
- Every connection is pinned to `DATABASE_SCHEMA`, so the six DCFS tables
  (`nodes`, `file_versions`, `file_chunks`, `stored_objects`, `sessions`)
  cannot collide with tables another application owns.
- `migrations/0001_initial.sql` is **additive and idempotent**: every statement
  is `IF NOT EXISTS` and it contains no `DROP`, `ALTER`, `TRUNCATE` or `DELETE`.
  Running it twice, or against a database that already has other tables, changes
  nothing else. A test enforces both properties.
- Schema installation is **opt-in**. Without `DATABASE_AUTO_MIGRATE=true` the
  server checks its tables at startup and, if they are missing, exits naming
  them and the command to run — it will not silently alter your database.

To install the schema yourself instead:

```bash
psql "$DATABASE_URL" -c "CREATE SCHEMA IF NOT EXISTS dcfs"
psql "$DATABASE_URL" -c "SET search_path TO dcfs" -f migrations/0001_initial.sql
```

**Rollback** is dropping those tables, and the schema if DCFS created
it. Nothing outside them is modified. Give DCFS a role scoped to its own
schema so that stays true by construction.

## API

Everything under `/api` requires `Authorization: Bearer $API_TOKEN`. Health endpoints do
not, so probes work without the secret.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health`, `/health/ready` | Liveness and readiness |
| `GET` | `/api/v1/fs` | Part size, so a client can align its writes |
| `POST` / `DELETE` | `/api/v1/sessions`, `/api/v1/sessions/:id` | Issue and revoke short-lived credentials (bootstrap token only) |
| `GET` | `/api/v1/nodes/root`, `/api/v1/nodes/:id`, `/api/v1/nodes/:id/children`, `/api/v1/nodes/:id/children/:name`, `/api/v1/nodes/resolve` | Namespace reads |
| `POST` | `/api/v1/nodes`, `/api/v1/nodes/:id/rename` | Create, rename |
| `PATCH` / `DELETE` | `/api/v1/nodes/:id` | Set attributes (including truncate), unlink |
| `GET` / `PUT` | `/api/v1/nodes/:id/data?offset=&size=` | Byte-level read and write |
| `POST` | `/api/v1/nodes/:id/sync` | Flush (writes are already durable on return) |
| `POST` | `/api/v1/versions/stage`, `/api/v1/versions/:id/chunks`, `/api/v1/versions/:id/commit` | Chunk-level write path |
| `GET` | `/api/v1/versions/:id`, `/api/v1/versions/:id/chunks`, `/api/v1/objects/:id` | Version and object reads |

Filenames travel as unpadded base64url, because Linux filenames are arbitrary bytes.

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

`scripts/check.sh` runs all three as one gate.

Both mount scripts take `PROFILE=release`. Use it for anything where throughput matters:
a debug build runs the encryption and hashing an order of magnitude slower, and some
races only appear once the code is fast enough to hit them.

## Development

### Project Structure

```
dcfs-rs/
├── crates/
│   ├── dcfs-core/          # Domain types, chunk planning, errors, retry logic
│   ├── dcfs-protocol/      # Wire types (JSON DTOs, base64url names)
│   ├── dcfs-server/        # Axum HTTP API server
│   ├── dcfs-db/            # PostgreSQL repository + in-memory impl
│   ├── dcfs-objectstore/   # Immutable object storage trait + implementations
│   ├── dcfs-crypto/        # XChaCha20-Poly1305 encryption with BLAKE3
│   ├── dcfs-fuse/          # FUSE filesystem client
│   ├── dcfs-discord/       # Discord API client (webhook attachments)
│   ├── dcfs-cache/         # Write-back cache with crash journal
│   └── dcfs-cli/           # Operator CLI: config-check, status
├── migrations/                  # SQL migrations (idempotent, additive)
├── scripts/                     # Smoke tests, workload scripts
└── docs/                        # Design docs and specifications
```

### Running Tests

```bash
# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p dcfs-core
cargo test -p dcfs-server

# Run integration tests
cargo test --test integration
cargo test --test e2e

# Run benchmarks
cargo bench -p dcfs-core

# Run PostgreSQL tests (requires running database)
TEST_DATABASE_URL=postgresql://postgres:***@localhost:55432/postgres cargo test --workspace
```

### Code Quality

```bash
# Format code
cargo fmt --all

# Check formatting
cargo fmt --all -- --check

# Run clippy lints
cargo clippy --workspace --all-targets -- -D warnings

# Run all checks (format + clippy + test)
scripts/check.sh
```

### Adding New Features

1. **Domain types** → `dcfs-core` (no I/O, pure Rust)
2. **Wire format** → `dcfs-protocol` (JSON serialization)
3. **Database** → `dcfs-db` (repository trait + SQLx implementation)
4. **Server endpoints** → `dcfs-server` (Axum handlers)
5. **Client operations** → `dcfs-fuse` (FUSE operations)
6. **Tests** → Add unit tests in `src/` and integration tests in `tests/`

### Debugging

```bash
# Enable debug logging
RUST_LOG=debug cargo run --bin dcfs-server

# Enable FUSE debug mode
dcfs-fuse /mnt/dcfs -d

# View server logs with timestamps
RUST_LOG=dcfs=debug,tower_http=debug cargo run --bin dcfs-server

# Check database state
psql "$DATABASE_URL" -c "SELECT * FROM dcfs.nodes LIMIT 10"
```

### Performance Profiling

```bash
# Run benchmarks
cargo bench

# Profile with perf (Linux)
perf record -g cargo run --release --bin dcfs-server
perf report

# Generate flamegraph
cargo flamegraph --bin dcfs-server
```

### Test layout

| Suite | What it covers |
|---|---|
| unit tests in `crates/*/src/**` | Domain invariants, chunk planning, crypto, cache state, rate limiting, config redaction, constant-time token comparison |
| `dcfs-core/tests/domain.rs` | Name, chunk and version-state invariants |
| `dcfs-protocol/tests/json_contract.rs` | Wire format, including non-UTF-8 names |
| `dcfs-db/tests/migration_contract.rs` | The migration keeps its required tables, constraints and indexes |
| `dcfs-server/tests/api_metadata.rs` | Namespace API, including `rmdir` semantics |
| `dcfs-server/tests/api_data.rs` | Byte I/O: chunk boundaries, unaligned overwrites, holes, chunk reuse, one version per write |
| `dcfs-server/tests/api_segment_read.rs` | A small read fetches one segment, not the whole part |
| `scripts/fuse-scale.sh` | How write cost grows with file size, through a real mount |
| `dcfs-server/tests/api_versions.rs` | Staging, manifests, atomic commit, conflict handling, encryption at rest |
| `dcfs-server/tests/api_auth.rs` | Every API route is behind the bearer token |
| `dcfs-server/tests/api_gc.rs` | Deletion and overwrite collect bytes; shared and live objects survive |
| `dcfs-server/tests/api_partial_read.rs` | A range read fetches only the parts it covers |
| `dcfs-fuse/tests/service.rs` | Filesystem semantics, write buffering and crash recovery, with no kernel involved |
| `dcfs-fuse/tests/reconnect.rs` | Riding out a server that is briefly unavailable, and the replay rules that make it safe |
| `dcfs-db/tests/postgres_repository.rs` | `PgRepository` against real PostgreSQL |
| `dcfs-server/tests/e2e_postgres.rs` | The real router over real PostgreSQL, end to end |
| `dcfs-fuse` unit tests | Block cache: fetch-once, block stitching, LRU eviction, version invalidation |
| `scripts/fuse-smoke.sh` | A real mount driven by `cp`, `dd`, `mv`, `rm`, with checksums. `MODE=mirror` runs the same suite mirrored |
| `scripts/fuse-workload.sh` | A real mount driven by git, sqlite, tar, rsync, symlinks, a 50 MB file and 8 concurrent writers |
| `scripts/fuse-outage.sh` | The server killed and restarted in the middle of a 30 MB copy |
| `dcfs-discord/tests/live_discord.rs` | The object-store contract against **real Discord** |
| `scripts/discord-e2e.sh` | A server backed by **real Discord**: write, restart, read, partial read, delete |

The PostgreSQL suites are skipped unless `TEST_DATABASE_URL` points at a **throwaway**
database — each test creates and drops its own schema, so never point it at real data:

```bash
docker run -d --rm -e POSTGRES_PASSWORD=dev -p 55432:5432 --name dfs-pg postgres:16
TEST_DATABASE_URL=postgresql://postgres:dev@localhost:55432/postgres cargo test --workspace
```

Both mount scripts need Linux with `/dev/fuse`; `fuse-smoke.sh` skips cleanly elsewhere.
They start their own server, so give them a throwaway `DATABASE_URL` too. What the
workload covers, beyond the smoke test: `git init`/`add`/`commit`/`fsck` on the mount, a
sqlite database with 500 inserts and `PRAGMA integrity_check`, tar and rsync round trips
that preserve symlinks, a 50 MB file with random reads and a mid-file overwrite, names
with spaces, emoji and raw non-UTF-8 bytes, eight concurrent writers, and a remount.

## Large files, parts and deletion

A file is split into fixed-size parts — `CHUNK_SIZE`, 8 MiB by default — and each part
becomes one immutable, separately encrypted object. Three consequences:

**Reads fetch only the parts they cover.** Reading ten bytes from the middle of a large
file fetches one part, not the file; a read straddling a boundary fetches two. Editing
one part re-encrypts and re-uploads that part alone and carries the rest over by
reference. `crates/dcfs-server/tests/api_partial_read.rs` counts the backend fetches
to hold this.

**Deleting a file deletes its bytes.** `rm` unlinks immediately, and a background sweep
then deletes the objects the file's versions referenced — the call that removes the
Discord attachment once that backend is wired in. Objects are shared between versions, so
the sweep only deletes an object that *no* live version still references; the test
`an_object_two_versions_share_survives_until_both_are_dead` pins that down. Overwriting
collects the superseded parts the same way.

Nothing is collected until it has been dead for `GC_RETENTION_SECS` (default one hour),
and the object store is emptied before the metadata that points at it, so a crash
mid-sweep leaves a stale row the next sweep cleans up rather than an object nothing
remembers.

**Writes are aligned to parts.** The client learns the server's part size at mount
(`GET /api/v1/fs`) and flushes buffered writes where a run ends on a part boundary. A
write that covers a whole part replaces it outright, so the server skips downloading and
decrypting the old one to merge into it. On a 50 MB copy that is the difference between
12.9 s and 6.7 s in a debug build, and 1.3 s in a release build.

**A write builds one version, and closing the file commits it.** Each request
adds to the same staging version; `sync` — which the FUSE client sends on
`flush` and `fsync`, so on every `close` — makes it the file. Committing per
request instead meant every request copied the whole manifest of the one
before it, so writing an N-part file wrote N(N+1)/2 chunk rows: 1.8 million for
a 30 GB file rather than 1,920. A long write also commits every gigabyte, which
bounds both how much an abandoned write loses and how long its version can sit
looking abandoned to the collector.

A file being written is readable straight away — the bytes were accepted, so
reads and `stat` account for them — but it is not durable until it is closed.
If the writer disappears first, the collector eventually drops the uncommitted
parts and puts the file's size back to what its last committed version holds.

**Several webhooks can share the load, one file at a time.** `DISCORD_WEBHOOKS`
takes `id:token` pairs separated by commas, and `DISCORD_WEBHOOK_ID` /
`DISCORD_WEBHOOK_TOKEN` is the first of them. Each webhook is rate limited on
its own, so more of them raise the ceiling on concurrent uploads — measured, one
webhook sustained 2.34 MB/s, which is what a 30 GB file took 3.8 hours to move
through.

A file goes to one webhook in its entirety. Splitting it would buy nothing, as a
single upload is bounded by the link rather than the webhook, and would scatter
one file over several channels. Which webhook holds an object is recorded with
its locator, because a webhook can only fetch or delete what it posted itself:
placement is a hash of the file id, but retrieval never is — webhooks are added
and removed, and the hash would then point where the object is not. An object
written before webhooks were named reads back through the first one, which is
the one that wrote it.

**One server serves many clients, with one caveat that matters.** Concurrent
writers to the same file are serialised per file and both land; two writing the
same place is last-write-wins, as on a local filesystem, never a splice of the
two. A write in progress is visible to every client, not isolated to the writer.

Cached blocks are keyed by version, and a committed version never changes, so a
mount that learns of a new version misses its cache rather than serving what the
version before it held. A read carries `x-dfs-committed`, and bytes from a write
still in progress are served but never kept — the version id does not move while
a write is open, so caching them would leave a client answering with content the
file may never end up holding.

**More than one server can share a database.** A writer takes an advisory lock
on the file from PostgreSQL, so two instances adding to the same file join one
open write instead of each starting their own and orphaning the other's parts.
The lock is released when the guard drops, including when a write fails partway.
Nothing about a write is kept in a server's memory: the point to commit at is
read from the sizes in the database, not counted per session, so it is the same
answer whichever instance asks.

The sweeper takes a lock of its own, and takes it without waiting: an instance
that finds another one sweeping skips that tick rather than queueing behind it,
because the sweep already running covers the same work. Two at once would read
the same batch and race to delete it — the same end state reached twice, with
the loser logging failures for work already done.

The lock holds a pooled connection for the length of the write, and a write
includes uploading to the backend — seconds, over Discord. Size the pool for the
writers you expect to overlap, or they will queue on connections rather than on
the lock. With the in-memory metadata store there is one process by definition
and the lock is a no-op.

What is left is the kernel's attribute TTL: a mount goes on using the version it
last looked up until those attributes expire, so another client's commit takes
up to that long to be noticed. Bounded staleness, as on any network filesystem —
not indefinite.

**An upload that breaks partway can be carried on.** A write stays open across
requests, and the file reports the size it has actually taken, so a client that
died resumes by asking for that size and writing on from it — no part is sent
twice and nothing starts over. If the collector reached the abandoned write
first, the size falls back to the last committed gigabyte and resuming from
*that* leaves no hole either. Measured: one 30 GB file takes 3.8 hours to
upload, which is long enough that this matters.

**A part is sealed as segments, so a small read costs a segment.** Parts are
sized for the backend's upload limit, so they are large — 16 MiB by default.
Sealing one as a single AEAD message meant a 4 KiB read had to fetch and
authenticate all 16 MiB of it. Each 256 KiB segment now carries its own tag,
and a read fetches only the segments it covers: 64x amplification instead of
4096x, and a random 4 KiB read measured 57 ms before and 10 ms after. Segments
are bound to their index and to the object, so they cannot be reordered,
dropped, or moved between objects. A range is authenticated by the segments it
touches rather than against the whole part's plaintext hash, which would mean
reading the whole part to check it.

Objects written before this layout have one tag over everything and cannot be
opened in part; they still read, by falling back to the whole-part path.

A signed attachment link carries its own expiry, and a link already known to
be dead is replaced before a download is spent discovering it. Reading a file
stored more than a day ago used to cost one failed download per part on top of
the refresh; it now costs the refresh alone. A link whose expiry cannot be read
is used as before and refreshed only if it fails, so this is an optimisation
and never what correctness rests on.

Discord's CDN answers `Range` with 206, so this saves the download and not
just the decrypt: on a real 16 MiB attachment, answering a 4 KiB read took
2.56-3.65 s fetching the whole part and 0.185-0.365 s fetching one segment. A
backend that ignores the header answers 200 with everything, which the store
slices, so correctness never depends on it.

**The parts a read spans are fetched at the same time.** Over a backend where
every part is a network round trip, doing them one after another makes a read
as slow as the sum of its parts instead of the slowest of them.

**Part size must fit the backend's upload limit.** A sealed part is
`CHUNK_SIZE + 40` bytes: a 24-byte nonce plus a 16-byte authentication tag. Set
`MAX_ATTACHMENT_BYTES` to whatever your backend accepts and the server checks the
arithmetic at startup, rather than failing on every upload. Discord's own limit depends
on the server's boost tier and on Discord's current policy, so nothing here hard-codes a
number.

## Troubleshooting

### Server won't start

**Missing environment variables:**
```bash
# Server refuses to start without these:
export MASTER_KEY="$(openssl rand -hex 32)"
export API_TOKEN="$(openssl rand -hex 32)"
export OBJECT_STORE_PATH="$PWD/data/objects"
export DATABASE_URL="postgresql://postgres:***@localhost:55432/dcfs"
```

**Database connection failed:**
```bash
# Check PostgreSQL is running
pg_isready -h localhost -p 5432

# Test connection
psql "$DATABASE_URL" -c "SELECT 1"

# Check if schema exists
psql "$DATABASE_URL" -c "SELECT table_name FROM information_schema.tables WHERE table_schema = 'dcfs'"
```

**Port already in use:**
```bash
# Check what's using port 3000
lsof -i :3000
# Or change the port
export SERVER_PORT=3001
```

### FUSE mount fails

**Permission denied:**
```bash
# Check FUSE is installed
ls -l /dev/fuse

# Check user is in fuse group
groups | grep fuse

# Add user to fuse group (requires logout/login)
sudo usermod -aG fuse $USER
```

**Mount point not empty:**
```bash
# FUSE requires an empty mount point
mkdir -p /tmp/dcfs
# or
rm -rf /tmp/dcfs/*
```

**Module not loaded (Linux):**
```bash
sudo modprobe fuse
```

### Performance issues

**Slow writes:**
- Use `--release` build: `cargo build --release`
- Check `CHUNK_SIZE` matches your workload (larger = fewer uploads, more memory)
- Monitor disk I/O: `iostat -x 1`

**Slow reads:**
- Use `mirror` mode for frequently accessed files
- Increase cache size: `--cache-size 4294967296` (4GB)
- Check network latency to object store

**High memory usage:**
- Reduce `--cache-size`
- Use `stream` mode instead of `mirror`
- Check for memory leaks with `valgrind` or `heaptrack`

### Data corruption

**Checksum mismatch:**
```bash
# Verify file integrity
sha256sum /mnt/dcfs/path/to/file

# Check server logs for encryption errors
journalctl -u dcfs-server | grep -i "checksum\|decrypt"
```

**Missing files after restart:**
- Content cache is not persistent (by design)
- Files are safe in PostgreSQL + object store
- Remount and access files to re-cache them

### Database issues

**Schema migration failed:**
```bash
# Check current schema version
psql "$DATABASE_URL" -c "SELECT version FROM dcfs.schema_migrations"

# Manual migration (if needed)
psql "$DATABASE_URL" -f migrations/0001_initial.sql
```

**Connection pool exhausted:**
```bash
# Increase pool size
export DATABASE_POOL_SIZE=20

# Check active connections
psql "$DATABASE_URL" -c "SELECT count(*) FROM pg_stat_activity WHERE datname = 'dcfs'"
```

### Object store issues

**Disk full:**
```bash
# Check object store size
du -sh "$OBJECT_STORE_PATH"

# Run garbage collection
curl -X POST http://localhost:3000/api/v1/gc/run -H "Authorization: Bearer ***
```

**Permission denied:**
```bash
# Check ownership
ls -ld "$OBJECT_STORE_PATH"

# Fix permissions
sudo chown -R $USER:$USER "$OBJECT_STORE_PATH"
```

### Getting help

1. Check logs: `RUST_LOG=debug` for verbose output
2. Run smoke tests: `scripts/fuse-smoke.sh`
3. Run workload tests: `scripts/fuse-workload.sh`
4. Open an issue: https://github.com/yourusername/dcfs-rs/issues

## Testing against real Discord

Everything else runs against a mock. These two need a real webhook, and they
**post real attachments** to the channel it belongs to, then delete them. Point
them at a channel you do not mind writing to.

Create a webhook in the channel's *Integrations* settings. The URL it gives you
is `https://discord.com/api/webhooks/<id>/<token>` — the last two path segments
are the two values below. **The token is a credential**: anyone holding it can
post to that channel, so keep it out of your shell history, your commits and
your terminal scrollback.

```bash
# git already ignores .env.*
printf 'DISCORD_WEBHOOK_ID=...\nDISCORD_WEBHOOK_TOKEN=...\n' > .env.discord-test
chmod 600 .env.discord-test
set -a; . ./.env.discord-test; set +a

cargo test -p dcfs-discord --test live_discord -- --test-threads=1
DATABASE_URL=postgresql://... scripts/discord-e2e.sh
```

Both skip cleanly when those variables are unset, and neither is part of CI. What they
covered on the run that has happened: a filesystem mounted with Discord as the object
store, a 5 MB file copied as five 1 MiB attachments with a matching checksum, an
unaligned overwrite, an unmount and remount, and `rm` followed by the collector deleting
the attachments — confirmed by asking Discord for the message afterwards and getting a
404.
The client scrubs the token out of anything it logs or returns, because Discord
puts it in the URL and a connection error would otherwise print a working
credential.

If a test fails with the part being refused, lower `CHUNK_SIZE`: Discord's
per-attachment limit depends on the server's boost tier and on Discord's
current policy, and a sealed part is `CHUNK_SIZE + 40` bytes.

## Losing the network

A mount is a filesystem: an application writing to it cannot retry for itself,
so a moment without connectivity has to look like a slow write rather than a
failed one. Three things carry that.

**Requests are retried.** Both hops — the FUSE client to the server, and the
server to Discord — retry a request that got no response, a 429, or a 5xx, with
exponential backoff and jitter. The FUSE client gives up after about 16 seconds
and returns an error to the kernel, because blocking a process forever is worse
than telling it the truth.

**Retries are safe to replay.** A read is trivially so. A write carries its own
offset, so sending the same bytes twice lands them in the same place. A create
carries an idempotency key that the server uses as the new node's id, so a
replay collides with itself — and the client resolves that by fetching the node
and checking it is the one it meant to create, rather than reporting a conflict
that never happened. A delete replayed after it succeeded finds nothing left,
which is the outcome that was asked for.

**Accepted writes are not lost.** Writes are coalesced before they are sent, so
a `write` returns success while the bytes are still in this process. Each
buffered run is also written to a journal on disk, and anything the journal
still holds is replayed at the next mount, before the filesystem serves anyone.
That covers the process dying; it is not `fsync`ed per run, so it does not
cover the machine losing power.

What is *not* resumable: a single part in flight. A failed `PUT` of one part is
retried in full, so an outage costs re-sending at most `CHUNK_SIZE` bytes, not
the file.

## Current limitations

- **The Discord backend has been run against live Discord once, by hand.** A mounted
  filesystem whose chunks were real attachments passed the whole smoke suite, including
  a multi-part copy, an unaligned overwrite, an unmount/remount and collection of the
  deleted attachments. It is not exercised on every change, because CI has no webhook:
  `scripts/discord-e2e.sh` and `live_discord.rs` are opt-in. Nothing is known about how
  it behaves at scale, over days, or against a rate limit that actually bites.
- **One namespace, no per-file authorization.** Sessions expire and can be revoked
  individually, but every credential reaches every file; there are no users.
- **No TLS.** Put the server behind a reverse proxy before it leaves localhost.
- **No key rotation.** Every chunk is sealed under `MASTER_KEY`; changing it makes
  existing files undecryptable.
- **The content cache does not survive a restart.** It starts empty on every mount, so a
  mirror re-fetches everything on the next start. Buffered *writes* do survive: they are
  journalled to disk and replayed at the next mount, though without an `fsync` per run,
  so they survive the process dying rather than the machine losing power.
- **Mirror mode is read-side only.** It keeps a full local copy for reads, but writes
  still go straight to the server rather than syncing from a local copy, and there is no
  offline write queue.
- **An outage longer than the retry window surfaces as an I/O error.** The window is
  fixed in the client rather than configurable, and a write that fails that way is
  reported to the application, not queued for later.
- **No hard links, xattrs, POSIX ACLs, device nodes, sockets or named pipes**, and no
  macOS or Windows support. Symbolic links are supported.
- **Renaming a directory onto a file, or a file onto a directory, returns EINVAL** where
  POSIX asks for ENOTDIR or EISDIR. The operation fails either way.
- **Single writer assumed.** Two servers against one database stay correct — the commit
  guard sees to that — but they will conflict often, because the per-file write lock is
  process-local.

## License

Dual-licensed under MIT OR Apache-2.0.
