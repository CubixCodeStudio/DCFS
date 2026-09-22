-- DCFS Initial Schema
-- Version: 0001
--
-- Additive and idempotent: every statement is IF NOT EXISTS, so applying this
-- to a database that already has the schema is a no-op. It never drops or
-- alters anything, which is what makes it safe to run against a database that
-- holds other applications' tables. Objects land in the current search_path,
-- so set DATABASE_SCHEMA to keep DCFS out of `public` on a shared server.

-- Nodes table: filesystem tree
CREATE TABLE IF NOT EXISTS nodes (
    id UUID PRIMARY KEY,
    parent_id UUID REFERENCES nodes(id) ON DELETE CASCADE,
    name BYTEA NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    mode INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    size BIGINT NOT NULL DEFAULT 0,
    atime TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    mtime TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ctime TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    current_version_id UUID,
    generation BIGINT NOT NULL DEFAULT 0,
    deleted_at TIMESTAMPTZ
);

-- Live names are unique per directory; soft-deleted rows free the name again.
-- (A table-level UNIQUE cannot carry a WHERE clause, hence the partial index.)
CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_live_name ON nodes (parent_id, name) WHERE deleted_at IS NULL;

-- Index for efficient directory listing
CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent_id) WHERE deleted_at IS NULL;

-- File versions table
CREATE TABLE IF NOT EXISTS file_versions (
    id UUID PRIMARY KEY,
    node_id UUID NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    base_version_id UUID REFERENCES file_versions(id),
    state TEXT NOT NULL CHECK (state IN ('staging', 'committed', 'superseded', 'garbage')),
    size BIGINT NOT NULL,
    plaintext_hash TEXT NOT NULL,
    chunk_size BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    committed_at TIMESTAMPTZ
);

-- Index for finding staging versions
CREATE INDEX IF NOT EXISTS idx_file_versions_node_state ON file_versions(node_id, state);

-- File chunks table
CREATE TABLE IF NOT EXISTS file_chunks (
    version_id UUID NOT NULL REFERENCES file_versions(id) ON DELETE CASCADE,
    chunk_index BIGINT NOT NULL,
    logical_offset BIGINT NOT NULL,
    plaintext_size INTEGER NOT NULL,
    plaintext_hash TEXT NOT NULL,
    object_id UUID NOT NULL,
    PRIMARY KEY (version_id, chunk_index)
);

-- Stored objects (remote encrypted chunks)
CREATE TABLE IF NOT EXISTS stored_objects (
    id UUID PRIMARY KEY,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    attachment_id TEXT NOT NULL,
    ciphertext_size BIGINT NOT NULL,
    crypto_key_id TEXT NOT NULL,
    nonce BYTEA NOT NULL,
    object_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMPTZ
);

-- Sessions for authentication
CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    user_id TEXT NOT NULL,
    namespace_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ
);

-- Index for finding active sessions
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id, expires_at) WHERE revoked_at IS NULL;
