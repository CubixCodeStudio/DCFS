//! Server configuration loaded from the environment.
//!
//! Secrets live only in this struct and never in logs: `Debug` redacts them, and
//! nothing here is echoed back over HTTP.

use std::env::{self, VarError};
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;

/// Default part size, also the value documented in `.env.example`.
///
/// Measured against a plain Discord webhook: 20 MiB was accepted and 22 MiB
/// came back as `40005 Request entity too large`. 16 MiB leaves room for the
/// 40 bytes a sealed part adds and for the multipart framing around it, and
/// halves the number of attachments a file costs compared with 8 MiB. A server
/// whose tier allows more can raise it; `MAX_ATTACHMENT_BYTES` checks the
/// arithmetic either way.
pub const DEFAULT_CHUNK_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{var} is not valid UTF-8")]
    NotUnicode { var: &'static str },
    #[error("{var} is invalid: {reason}")]
    Invalid { var: &'static str, reason: String },
}

/// A secret that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Resolved server configuration.
pub struct Config {
    /// PostgreSQL connection string. `None` selects the in-memory repository,
    /// which is for development and tests only — nothing survives a restart.
    pub database_url: Option<Secret>,
    /// Shared bearer token every client must present.
    pub api_token: Option<Secret>,
    /// Discord webhook credentials. The client posts attachments as a webhook,
    /// which is what these are — not a bot token.
    pub discord_webhook_id: Option<String>,
    pub discord_webhook_token: Option<Secret>,
    /// Extra webhooks, each `id:token`.
    ///
    /// Every webhook is rate limited on its own, so several of them raise the
    /// ceiling on how much can be uploaded at once. A file always goes to one
    /// of them in its entirety — splitting it would buy nothing and scatter it
    /// over several channels.
    pub discord_webhooks: Vec<(String, Secret)>,
    /// 32-byte master key used to derive per-chunk keys.
    pub master_key: Option<[u8; 32]>,
    pub server_addr: SocketAddr,
    /// PostgreSQL schema DiscordFS lives in. Lets it share a database with
    /// other applications instead of taking over `public`.
    pub database_schema: String,
    /// Install the schema at startup if it is missing. Off by default: the
    /// target may be someone else's database.
    pub database_auto_migrate: bool,
    /// Directory holding encrypted chunks.
    pub object_store_path: Option<std::path::PathBuf>,
    pub chunk_size: u64,
    /// Largest object the storage backend will accept, if it has a limit.
    /// `CHUNK_SIZE` plus the encryption overhead must fit inside it.
    pub max_attachment_bytes: Option<u64>,
    pub gc_retention: Duration,
}

impl Config {
    /// Read configuration from the process environment.
    ///
    /// Reads the environment directly; there is no `.env` file parsing, so export
    /// the variables or run the process under a tool that does it.
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url = optional_var("DATABASE_URL")?.map(Secret);
        let api_token = match optional_var("API_TOKEN")? {
            // A short token is worse than none, because it looks like security.
            Some(token) if token.len() < 16 => {
                return Err(ConfigError::Invalid {
                    var: "API_TOKEN",
                    reason: "must be at least 16 characters".to_string(),
                })
            }
            other => other.map(Secret),
        };
        let discord_webhook_id = optional_var("DISCORD_WEBHOOK_ID")?;
        let discord_webhook_token = optional_var("DISCORD_WEBHOOK_TOKEN")?.map(Secret);
        // DISCORD_WEBHOOKS=id:token,id:token — the pair above is the first of
        // them, kept because it is what every existing deployment sets.
        let mut discord_webhooks = Vec::new();
        if let (Some(id), Some(token)) = (&discord_webhook_id, &discord_webhook_token) {
            discord_webhooks.push((id.clone(), Secret(token.expose().to_string())));
        }
        for entry in optional_var("DISCORD_WEBHOOKS")?
            .unwrap_or_default()
            .split(',')
        {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((id, token)) = entry.split_once(':') else {
                return Err(ConfigError::Invalid {
                    var: "DISCORD_WEBHOOKS",
                    reason: "each entry is id:token, separated by commas".to_string(),
                });
            };
            if discord_webhooks.iter().any(|(known, _)| known == id) {
                continue;
            }
            discord_webhooks.push((id.to_string(), Secret(token.to_string())));
        }

        if discord_webhook_id.is_some() != discord_webhook_token.is_some() {
            return Err(ConfigError::Invalid {
                var: "DISCORD_WEBHOOK_ID",
                reason: "set both DISCORD_WEBHOOK_ID and DISCORD_WEBHOOK_TOKEN, or neither"
                    .to_string(),
            });
        }

        let master_key = match optional_var("MASTER_KEY")? {
            None => None,
            Some(hex) => {
                let hex = hex.trim();
                if hex.len() != 64 {
                    return Err(ConfigError::Invalid {
                        var: "MASTER_KEY",
                        // Never quote the value itself.
                        reason: format!("expected 64 hex chars (32 bytes), got {}", hex.len()),
                    });
                }
                let mut key = [0u8; 32];
                for (i, byte) in key.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
                        ConfigError::Invalid {
                            var: "MASTER_KEY",
                            reason: "not valid hex".to_string(),
                        }
                    })?;
                }
                Some(key)
            }
        };

        // Default to loopback: the API has no authentication yet.
        let server_addr = match optional_var("SERVER_ADDR")? {
            None => SocketAddr::from(([127, 0, 0, 1], 8080)),
            Some(raw) => raw.parse().map_err(|e| ConfigError::Invalid {
                var: "SERVER_ADDR",
                reason: format!("{raw:?}: {e}"),
            })?,
        };

        let database_schema = optional_var("DATABASE_SCHEMA")?.unwrap_or_else(|| "public".into());
        let database_auto_migrate = match optional_var("DATABASE_AUTO_MIGRATE")?.as_deref() {
            None => false,
            Some("true" | "1" | "yes") => true,
            Some("false" | "0" | "no") => false,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    var: "DATABASE_AUTO_MIGRATE",
                    reason: format!("expected true or false, got {other:?}"),
                })
            }
        };

        let object_store_path = optional_var("OBJECT_STORE_PATH")?.map(std::path::PathBuf::from);

        let chunk_size = match optional_var("CHUNK_SIZE")? {
            None => DEFAULT_CHUNK_SIZE,
            Some(raw) => {
                let n: u64 = raw.parse().map_err(|e| ConfigError::Invalid {
                    var: "CHUNK_SIZE",
                    reason: format!("{raw:?}: {e}"),
                })?;
                if n == 0 {
                    return Err(ConfigError::Invalid {
                        var: "CHUNK_SIZE",
                        reason: "must be greater than zero".to_string(),
                    });
                }
                n
            }
        };

        // Discord's per-attachment limit depends on the server's boost tier and
        // on Discord's current policy, so there is no safe number to hard-code:
        // the operator sets it to whatever their backend actually accepts, and
        // this only checks the arithmetic.
        let max_attachment_bytes = match optional_var("MAX_ATTACHMENT_BYTES")? {
            None => None,
            Some(raw) => {
                let max: u64 = raw.parse().map_err(|e| ConfigError::Invalid {
                    var: "MAX_ATTACHMENT_BYTES",
                    reason: format!("{raw:?}: {e}"),
                })?;
                let sealed = chunk_size + discordfs_crypto::CIPHERTEXT_OVERHEAD;
                if sealed > max {
                    return Err(ConfigError::Invalid {
                        var: "CHUNK_SIZE",
                        reason: format!(
                            "a {chunk_size}-byte chunk seals to {sealed} bytes, over the \
                             MAX_ATTACHMENT_BYTES limit of {max}; use at most {}",
                            max.saturating_sub(discordfs_crypto::CIPHERTEXT_OVERHEAD)
                        ),
                    });
                }
                Some(max)
            }
        };

        let gc_retention = match optional_var("GC_RETENTION_SECS")? {
            None => Duration::from_secs(3600),
            Some(raw) => Duration::from_secs(raw.parse().map_err(|e| ConfigError::Invalid {
                var: "GC_RETENTION_SECS",
                reason: format!("{raw:?}: {e}"),
            })?),
        };

        Ok(Self {
            database_url,
            api_token,
            discord_webhook_id,
            discord_webhook_token,
            discord_webhooks,
            master_key,
            server_addr,
            database_schema,
            database_auto_migrate,
            object_store_path,
            chunk_size,
            max_attachment_bytes,
            gc_retention,
        })
    }
}

/// Empty and unset are the same thing, so a blank line in `.env` is not a value.
fn optional_var(var: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(var) {
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode { var }),
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "<redacted>"),
            )
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field("discord_webhooks", &self.discord_webhooks.len())
            .field("discord_webhook_id", &self.discord_webhook_id)
            .field(
                "discord_webhook_token",
                &self.discord_webhook_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "master_key",
                &self.master_key.as_ref().map(|_| "<redacted>"),
            )
            .field("server_addr", &self.server_addr)
            .field("database_schema", &self.database_schema)
            .field("database_auto_migrate", &self.database_auto_migrate)
            .field("object_store_path", &self.object_store_path)
            .field("chunk_size", &self.chunk_size)
            .field("max_attachment_bytes", &self.max_attachment_bytes)
            .field("gc_retention", &self.gc_retention)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let config = Config {
            database_url: Some(Secret("postgresql://u:p@host/db".to_string())),
            api_token: Some(Secret("token-value-that-is-long".to_string())),
            discord_webhook_id: Some("123456".to_string()),
            discord_webhook_token: Some(Secret("token-value".to_string())),
            discord_webhooks: vec![("123456".to_string(), Secret("token-value".to_string()))],
            master_key: Some([7u8; 32]),
            server_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            database_schema: "public".to_string(),
            database_auto_migrate: false,
            object_store_path: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_attachment_bytes: None,
            gc_retention: Duration::from_secs(3600),
        };

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("token-value"));
        assert!(rendered.contains(r#"api_token: Some("<redacted>")"#));
        assert!(!rendered.contains("postgresql://"));
        assert!(rendered.contains(r#"master_key: Some("<redacted>")"#));
        assert!(format!("{:?}", Secret("s3cret".to_string())) == "<redacted>");
    }

    // The env is process-global, so the var-parsing cases share one test rather
    // than racing each other across threads.
    #[test]
    fn parses_and_validates_env_vars() {
        env::set_var("DFS_TEST_EMPTY", "   ");
        assert_eq!(optional_var("DFS_TEST_EMPTY").unwrap(), None);
        env::remove_var("DFS_TEST_EMPTY");

        env::set_var("MASTER_KEY", "ab".repeat(32));
        env::set_var("CHUNK_SIZE", "4096");
        env::set_var("SERVER_ADDR", "127.0.0.1:9999");
        env::set_var("DATABASE_SCHEMA", "discordfs");
        let config = Config::from_env().unwrap();
        assert_eq!(config.master_key.unwrap()[0], 0xab);
        assert_eq!(config.chunk_size, 4096);
        assert_eq!(config.server_addr.port(), 9999);
        assert_eq!(config.database_schema, "discordfs");
        assert!(!config.database_auto_migrate, "migrations stay opt-in");

        env::set_var("DATABASE_AUTO_MIGRATE", "maybe");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Invalid {
                var: "DATABASE_AUTO_MIGRATE",
                ..
            })
        ));
        env::remove_var("DATABASE_AUTO_MIGRATE");

        // A chunk must still fit once sealed.
        env::set_var("CHUNK_SIZE", "4096");
        env::set_var("MAX_ATTACHMENT_BYTES", "4096");
        assert!(
            matches!(
                Config::from_env(),
                Err(ConfigError::Invalid {
                    var: "CHUNK_SIZE",
                    ..
                })
            ),
            "a chunk exactly at the limit does not fit once sealed"
        );
        env::set_var("MAX_ATTACHMENT_BYTES", "8192");
        assert_eq!(Config::from_env().unwrap().max_attachment_bytes, Some(8192));
        env::remove_var("MAX_ATTACHMENT_BYTES");

        env::set_var("CHUNK_SIZE", "0");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Invalid {
                var: "CHUNK_SIZE",
                ..
            })
        ));

        env::set_var("CHUNK_SIZE", "4096");
        env::set_var("MASTER_KEY", "tooshort");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Invalid {
                var: "MASTER_KEY",
                ..
            })
        ));

        for var in ["MASTER_KEY", "CHUNK_SIZE", "SERVER_ADDR"] {
            env::remove_var(var);
        }
    }
}
