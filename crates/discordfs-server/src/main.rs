//! DiscordFS server binary.

use discordfs_db::{MemoryMetadataRepository, MetadataRepository, PgRepository};
use discordfs_discord::{DiscordClient, DiscordClientConfig, DiscordObjectStore};
use discordfs_objectstore::{FsObjectStore, ObjectStore};
use std::sync::Arc;
use std::time::Duration;

use discordfs_server::{build_router, config::Config, locators::RepositoryLocatorStore, AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Colour only for a terminal. Redirected to a file or a log collector,
        // the escape codes land between the field name and its value, where
        // they defeat anything that greps for one.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    // Config errors name the offending variable, never its value.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("invalid configuration: {e}");
            std::process::exit(1);
        }
    };
    tracing::debug!(?config, "loaded configuration");

    let repo: Arc<dyn MetadataRepository> = match &config.database_url {
        Some(url) => Arc::new(open_postgres(url.expose(), &config).await),
        None => {
            tracing::warn!(
                "DATABASE_URL is not set: using the in-memory repository, all metadata is lost on restart"
            );
            Arc::new(MemoryMetadataRepository::new())
        }
    };

    // Chunks must outlive the process: an object store that forgets them while
    // the metadata store keeps pointing at them reads as silent corruption
    // rather than as data loss, so there is no in-memory fallback here.
    let store: Arc<dyn ObjectStore> = if config.discord_webhooks.is_empty() {
        let Some(path) = config.object_store_path.clone() else {
            tracing::error!(
                "set OBJECT_STORE_PATH, or DISCORD_WEBHOOK_ID and DISCORD_WEBHOOK_TOKEN: \
                 chunks have to outlive the process"
            );
            std::process::exit(1);
        };
        match FsObjectStore::open(&path).await {
            Ok(store) => {
                tracing::info!("object store: {}", path.display());
                Arc::new(store)
            }
            Err(e) => {
                tracing::error!("cannot open OBJECT_STORE_PATH: {e}");
                std::process::exit(1);
            }
        }
    } else {
        let clients: Vec<DiscordClient> = config
            .discord_webhooks
            .iter()
            .map(|(id, token)| DiscordClient::new(DiscordClientConfig::new(id, token.expose())))
            .collect();
        // Locators live in the metadata store, so an attachment can still be
        // found after a restart — and which webhook holds it is recorded with
        // it, because only that webhook can fetch or delete it.
        let locators = Arc::new(RepositoryLocatorStore::new(repo.clone()));
        tracing::info!(webhooks = clients.len(), "object store: discord");
        Arc::new(DiscordObjectStore::with_clients(clients, locators))
    };

    // Fail closed: without a key the server would have to store plaintext, so
    // it refuses to start rather than quietly downgrading.
    let Some(master_key) = config.master_key else {
        tracing::error!("MASTER_KEY is required: generate one with `openssl rand -hex 32`");
        std::process::exit(1);
    };

    // Fail closed again: an unauthenticated filesystem API lets any local
    // process read or delete every file.
    let Some(api_token) = config.api_token else {
        tracing::error!("API_TOKEN is required: generate one with `openssl rand -hex 32`");
        std::process::exit(1);
    };

    let state = AppState::new(repo.clone(), store.clone(), master_key, config.chunk_size)
        .with_api_token(api_token.expose());

    // Sweep often enough that space comes back in a reasonable time, but never
    // more often than the retention window it enforces.
    // Never busier than once a second, however short the retention is.
    let gc_interval = config
        .gc_retention
        .min(Duration::from_secs(300))
        .max(Duration::from_secs(1));
    tracing::info!(
        retention_secs = config.gc_retention.as_secs(),
        interval_secs = gc_interval.as_secs(),
        "garbage collection enabled"
    );
    discordfs_server::gc::spawn(repo, store, config.gc_retention, gc_interval);

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(config.server_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {}: {e}", config.server_addr));

    tracing::info!("DiscordFS server listening on {}", config.server_addr);

    axum::serve(listener, app).await.expect("server error");
}

/// Connect to PostgreSQL and make sure DiscordFS's tables are reachable.
///
/// Designed to be pointed at a database that already exists and may already
/// hold other applications' tables: it never creates a database, it confines
/// itself to `DATABASE_SCHEMA`, and it installs its own tables only when
/// `DATABASE_AUTO_MIGRATE` says so. It exits rather than guessing.
async fn open_postgres(url: &str, config: &Config) -> PgRepository {
    // SQLx errors can echo the DSN back, so strip it before logging.
    let redact = |e: String| e.replace(url, "<DATABASE_URL>");

    let repo = match PgRepository::connect(url, 16, &config.database_schema).await {
        Ok(repo) => repo,
        Err(e) => {
            tracing::error!("cannot connect to DATABASE_URL: {}", redact(e.to_string()));
            std::process::exit(1);
        }
    };

    if config.database_auto_migrate {
        // Blast radius: creates the schema if missing, then adds DiscordFS's
        // tables and their indexes. Every statement is IF NOT EXISTS and
        // the migration contains no DROP, ALTER, TRUNCATE or DELETE, so
        // existing objects — DiscordFS's or anyone else's — are untouched.
        // To roll back, drop those six tables, and the schema if it was
        // created here; nothing else changed.
        tracing::info!(
            schema = %config.database_schema,
            "DATABASE_AUTO_MIGRATE is on: installing the DiscordFS tables if they are missing"
        );
        if let Err(e) = repo.ensure_schema(&config.database_schema).await {
            tracing::error!("cannot create the schema: {}", redact(e.to_string()));
            std::process::exit(1);
        }
        if let Err(e) = repo.migrate().await {
            tracing::error!("migration failed: {}", redact(e.to_string()));
            std::process::exit(1);
        }
    }

    match repo.missing_tables().await {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            tracing::error!(
                schema = %config.database_schema,
                "missing tables: {}. Apply the schema with \
                 `psql \"$DATABASE_URL\" -f migrations/0001_initial.sql`, or set \
                 DATABASE_AUTO_MIGRATE=true to let the server install them.",
                missing.join(", ")
            );
            std::process::exit(1);
        }
        Err(e) => {
            tracing::error!("cannot inspect the schema: {}", redact(e.to_string()));
            std::process::exit(1);
        }
    }

    if let Err(e) = repo.ensure_root().await {
        tracing::error!(
            "cannot read or create the root directory: {}",
            redact(e.to_string())
        );
        std::process::exit(1);
    }

    tracing::info!(schema = %config.database_schema, "metadata backend: postgresql");
    repo
}
