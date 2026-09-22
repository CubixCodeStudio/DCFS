//! DCFS database layer.
//!
//! PostgreSQL implementation of metadata repositories.

pub mod memory;
pub mod postgres;
pub mod repository;

pub use memory::*;
pub use postgres::PgRepository;
pub use repository::*;

/// Initial database migration SQL.
pub const MIGRATION_0001: &str = include_str!("../../../migrations/0001_initial.sql");

/// Adds symbolic links. Additive: a new nullable column and a widened CHECK.
pub const MIGRATION_0002: &str = include_str!("../../../migrations/0002_symlinks.sql");

/// Indexes the object back-reference that garbage collection walks.
pub const MIGRATION_0003: &str = include_str!("../../../migrations/0003_gc_index.sql");

/// Adds session credentials, and drops the unused `upload_jobs` table.
pub const MIGRATION_0004: &str = include_str!("../../../migrations/0004_sessions.sql");

/// Lets an object locator be stored for a backend that has no guild or channel.
pub const MIGRATION_0005: &str = include_str!("../../../migrations/0005_object_locators.sql");

/// Every migration, in order. Re-running the set is a no-op.
pub const MIGRATIONS: [&str; 5] = [
    MIGRATION_0001,
    MIGRATION_0002,
    MIGRATION_0003,
    MIGRATION_0004,
    MIGRATION_0005,
];
