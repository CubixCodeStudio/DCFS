//! DiscordFS HTTP server.
//!
//! The router is built from an [`AppState`] holding trait objects, so the same
//! routes run against the in-memory backends in tests and against PostgreSQL in
//! production.

pub mod auth;
pub mod config;
pub mod error;
pub mod gc;
pub mod handlers;
pub mod locators;
pub mod state;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use discordfs_db::{MemoryMetadataRepository, MetadataRepository};
use discordfs_objectstore::{memory::MemoryObjectStore, ObjectStore};
use std::sync::Arc;

pub use error::AppError;
pub use state::AppState;

/// Build the Axum router with all routes.
///
/// Health endpoints sit outside the auth layer so a probe does not need the
/// token; everything under `/api` requires it.
pub fn build_router(state: AppState) -> Router {
    let api = api_routes(state.clone()).layer(middleware::from_fn_with_state(
        state.clone(),
        auth::require_token,
    ));

    // Minting and revoking credentials needs the bootstrap token, so a leaked
    // session cannot extend or replace itself.
    let sessions = Router::new()
        .route("/api/v1/sessions", post(handlers::sessions::create_session))
        .route(
            "/api/v1/sessions/:id",
            delete(handlers::sessions::revoke_session),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state,
            auth::require_bootstrap_token,
        ));

    Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/health/ready", get(handlers::health::readiness_check))
        .merge(sessions)
        .merge(api)
}

fn api_routes(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/fs", get(handlers::nodes::get_fs_info))
        // Node endpoints
        .route("/api/v1/nodes/root", get(handlers::nodes::get_root))
        .route("/api/v1/nodes/:id", get(handlers::nodes::get_node))
        .route("/api/v1/nodes/:id", delete(handlers::nodes::delete_node))
        .route("/api/v1/nodes/:id", patch(handlers::nodes::patch_node))
        .route(
            "/api/v1/nodes/:id/rename",
            post(handlers::nodes::rename_node),
        )
        .route(
            "/api/v1/nodes/:id/children",
            get(handlers::nodes::list_children),
        )
        .route(
            "/api/v1/nodes/:id/children/:name",
            get(handlers::nodes::lookup_child),
        )
        .route("/api/v1/nodes", post(handlers::nodes::create_node))
        .route("/api/v1/nodes/resolve", get(handlers::nodes::resolve_path))
        // File data: the FUSE client reads and writes plain bytes here and
        // never handles a chunk, an object id or a key.
        .route("/api/v1/nodes/:id/data", get(handlers::data::read_data))
        .route(
            "/api/v1/nodes/:id/data",
            // The kernel can hand FUSE writes far larger than axum's 2 MiB
            // default, and a rejected body surfaces as an unexplained EIO.
            put(handlers::data::write_data).layer(DefaultBodyLimit::max(MAX_WRITE_BYTES)),
        )
        .route("/api/v1/nodes/:id/sync", post(handlers::data::sync_node))
        // Version endpoints
        .route(
            "/api/v1/versions/stage",
            post(handlers::versions::stage_version),
        )
        .route(
            "/api/v1/versions/:version_id/chunks",
            post(handlers::versions::upload_chunk).layer(DefaultBodyLimit::max(MAX_WRITE_BYTES)),
        )
        .route(
            "/api/v1/versions/:version_id/chunks",
            get(handlers::versions::list_chunks),
        )
        .route(
            "/api/v1/versions/:version_id/commit",
            post(handlers::versions::commit_version),
        )
        .route(
            "/api/v1/versions/:version_id",
            get(handlers::versions::get_version),
        )
        // Object endpoints
        .route("/api/v1/objects/:id", get(handlers::objects::get_object))
        .with_state(state)
}

/// Largest body accepted on the byte-write and chunk-upload routes.
pub const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;

/// Chunk size used by [`create_server`]: small enough that tests cross chunk
/// boundaries without moving megabytes around.
pub const TEST_CHUNK_SIZE: u64 = 64;

/// Build a router backed by in-memory storage and a fixed key.
///
/// Development and tests only: the key is a constant, so nothing it seals is
/// confidential.
pub fn create_server() -> Router {
    let repo: Arc<dyn MetadataRepository> = Arc::new(MemoryMetadataRepository::new());
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryObjectStore::new());
    build_router(AppState::new(repo, store, [0x42; 32], TEST_CHUNK_SIZE))
}
