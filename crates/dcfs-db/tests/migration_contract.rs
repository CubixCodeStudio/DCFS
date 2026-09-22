//! Migration contract tests.

use dcfs_db::MIGRATION_0001;

#[test]
fn migration_creates_nodes_table() {
    assert!(MIGRATION_0001.contains("CREATE TABLE IF NOT EXISTS nodes"));
    assert!(MIGRATION_0001.contains("id UUID PRIMARY KEY"));
    assert!(MIGRATION_0001.contains("parent_id UUID"));
    assert!(MIGRATION_0001.contains("name BYTEA NOT NULL"));
    assert!(MIGRATION_0001.contains("kind TEXT NOT NULL"));
}

#[test]
fn migration_creates_file_versions_table() {
    assert!(MIGRATION_0001.contains("CREATE TABLE IF NOT EXISTS file_versions"));
    assert!(MIGRATION_0001.contains("node_id UUID NOT NULL"));
    assert!(MIGRATION_0001.contains("state TEXT NOT NULL"));
    assert!(MIGRATION_0001.contains("size BIGINT NOT NULL"));
}

#[test]
fn migration_creates_file_chunks_table() {
    assert!(MIGRATION_0001.contains("CREATE TABLE IF NOT EXISTS file_chunks"));
    assert!(MIGRATION_0001.contains("version_id UUID NOT NULL"));
    assert!(MIGRATION_0001.contains("chunk_index BIGINT NOT NULL"));
    assert!(MIGRATION_0001.contains("object_id UUID NOT NULL"));
}

#[test]
fn migration_creates_stored_objects_table() {
    assert!(MIGRATION_0001.contains("CREATE TABLE IF NOT EXISTS stored_objects"));
    assert!(MIGRATION_0001.contains("guild_id TEXT NOT NULL"));
    assert!(MIGRATION_0001.contains("channel_id TEXT NOT NULL"));
    assert!(MIGRATION_0001.contains("message_id TEXT NOT NULL"));
    assert!(MIGRATION_0001.contains("attachment_id TEXT NOT NULL"));
}

#[test]
fn migration_has_unique_constraint_for_live_names() {
    assert!(MIGRATION_0001
        .contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_live_name ON nodes (parent_id, name) WHERE deleted_at IS NULL"));
}

#[test]
fn migration_has_check_constraints() {
    assert!(MIGRATION_0001.contains("CHECK (kind IN ('file', 'directory'))"));
    assert!(MIGRATION_0001
        .contains("CHECK (state IN ('staging', 'committed', 'superseded', 'garbage'))"));
}

/// Re-running the migration must not fail, so it can be pointed at a database
/// that already has the schema.
#[test]
fn migration_is_idempotent_and_never_destructive() {
    for statement in ["CREATE TABLE", "CREATE INDEX", "CREATE UNIQUE INDEX"] {
        for occurrence in MIGRATION_0001.match_indices(statement) {
            let tail = &MIGRATION_0001[occurrence.0 + statement.len()..];
            assert!(
                tail.starts_with(" IF NOT EXISTS"),
                "{statement} without IF NOT EXISTS near: {}",
                &tail[..tail.len().min(60)]
            );
        }
    }
    for destructive in ["DROP ", "TRUNCATE", "ALTER TABLE", "DELETE FROM"] {
        assert!(
            !MIGRATION_0001.contains(destructive),
            "the migration must never {destructive}"
        );
    }
}

#[test]
fn migration_creates_indexes() {
    assert!(MIGRATION_0001.contains("CREATE INDEX IF NOT EXISTS idx_nodes_parent"));
    assert!(MIGRATION_0001.contains("CREATE INDEX IF NOT EXISTS idx_file_versions_node_state"));
}

/// 0002 runs against databases that already hold data, so it may only widen
/// what is allowed. It adds a nullable column and relaxes a CHECK; it must not
/// drop a table or touch a row.
#[test]
fn migration_0002_only_widens() {
    use dcfs_db::MIGRATION_0002;

    assert!(MIGRATION_0002.contains("ADD COLUMN IF NOT EXISTS link_target BYTEA"));
    assert!(MIGRATION_0002.contains("CHECK (kind IN ('file', 'directory', 'symlink'))"));
    // A link carries a target and nothing else does.
    assert!(MIGRATION_0002.contains("CHECK ((kind = 'symlink') = (link_target IS NOT NULL))"));

    for destructive in [
        "DROP TABLE",
        "DROP COLUMN",
        "TRUNCATE",
        "DELETE FROM",
        "UPDATE ",
    ] {
        assert!(
            !MIGRATION_0002.contains(destructive),
            "0002 must never {destructive}"
        );
    }
    // Dropping a constraint before re-adding it is how a CHECK is widened, and
    // IF EXISTS keeps that idempotent.
    for dropped in MIGRATION_0002.match_indices("DROP CONSTRAINT") {
        let tail = &MIGRATION_0002[dropped.0 + "DROP CONSTRAINT".len()..];
        assert!(
            tail.starts_with(" IF EXISTS"),
            "constraint drops must be idempotent"
        );
    }
}

#[test]
fn migrations_are_listed_in_order() {
    use dcfs_db::{MIGRATIONS, MIGRATION_0001, MIGRATION_0002, MIGRATION_0003, MIGRATION_0004};
    assert_eq!(MIGRATIONS[0], MIGRATION_0001);
    assert_eq!(MIGRATIONS[1], MIGRATION_0002);
    assert_eq!(MIGRATIONS[2], MIGRATION_0003);
    assert_eq!(MIGRATIONS[3], MIGRATION_0004);
}

/// A session row must never be usable as a credential on its own.
#[test]
fn migration_0004_stores_only_a_token_hash() {
    use dcfs_db::MIGRATION_0004;
    assert!(MIGRATION_0004.contains("ADD COLUMN IF NOT EXISTS token_hash TEXT"));
    assert!(MIGRATION_0004.contains("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_token"));
    // The one drop is guarded on the table existing and being empty.
    assert!(MIGRATION_0004.contains("to_regclass('upload_jobs') IS NOT NULL"));
    assert!(MIGRATION_0004.contains("IF rows_present = 0 THEN"));
    for destructive in [
        "DROP TABLE nodes",
        "DROP TABLE file",
        "TRUNCATE",
        "DELETE FROM",
    ] {
        assert!(!MIGRATION_0004.contains(destructive));
    }
}

/// Garbage collection looks chunks up by object id; without this index both
/// the reference check and the delete scan the whole table.
#[test]
fn migration_0003_indexes_the_object_back_reference() {
    use dcfs_db::MIGRATION_0003;
    assert!(MIGRATION_0003
        .contains("CREATE INDEX IF NOT EXISTS idx_file_chunks_object ON file_chunks (object_id)"));
    for destructive in [
        "DROP TABLE",
        "DROP COLUMN",
        "TRUNCATE",
        "DELETE FROM",
        "ALTER TABLE",
    ] {
        assert!(!MIGRATION_0003.contains(destructive));
    }
}
