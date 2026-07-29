use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Database connection URL.
    /// Examples: "sqlite:orion.db", "postgres://user:pass@host/db", "mysql://user:pass@host/db"
    pub url: String,
    pub max_connections: u32,
    /// Minimum number of connections to maintain in the pool (0 = no minimum).
    pub min_connections: u32,
    /// SQLite busy timeout in milliseconds (ignored for other backends).
    pub busy_timeout_ms: u64,
    /// Connection pool acquire timeout in seconds.
    pub acquire_timeout_secs: u64,
    /// Maximum idle time in seconds before a connection is closed (0 = no limit).
    pub idle_timeout_secs: u64,
    /// Directory for database backup files (SQLite only).
    pub backup_dir: String,
    /// Keep only the newest N `orion_backup_*.db` files in `backup_dir`,
    /// pruning older ones after each successful backup. Unset (the default)
    /// keeps every backup — but they accumulate on the same disk as the live
    /// database, so bounded deployments should set this. Must be >= 1 when
    /// set (SQLite only).
    pub backup_retention_count: Option<u32>,
    /// Run pending migrations automatically at startup. Set `false` in
    /// multi-replica deployments so boot-racing replicas never migrate;
    /// run `orion-server migrate` as a deploy step instead. Startup fails
    /// hard when this is `false` and migrations are pending.
    pub auto_migrate: bool,
    /// How long to keep retrying the initial database connection at startup,
    /// in seconds (D14). `0` fails fast on the first error.
    ///
    /// A Postgres/MySQL failover takes tens of seconds; without a retry window
    /// every replica exits, and the container restart backoff then outlives
    /// the outage. Retries are bounded by this window with a fixed exponential
    /// backoff, and the *migration* check stays fail-fast. Ignored for SQLite,
    /// whose connect failures (bad path, permissions, corrupt file) are not
    /// transient.
    pub connect_retry_secs: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            url: "sqlite:orion.db".to_string(),
            max_connections: 50,
            min_connections: 5,
            busy_timeout_ms: 5000,
            acquire_timeout_secs: 3,
            idle_timeout_secs: 300,
            backup_dir: "./backups".to_string(),
            backup_retention_count: None,
            auto_migrate: true,
            connect_retry_secs: 60,
        }
    }
}

impl StorageConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonempty(&self.url, "storage.url")?;
        require_nonzero(self.busy_timeout_ms, "storage.busy_timeout_ms")?;
        require_nonzero(self.acquire_timeout_secs, "storage.acquire_timeout_secs")?;
        // 0 would delete the backup just written; "keep none" is not a
        // retention policy — leave the field unset to disable pruning.
        if self.backup_retention_count == Some(0) {
            return Err(OrionError::Config {
                message: "storage.backup_retention_count must be >= 1 when set \
                          (unset keeps every backup)"
                    .to_string(),
            });
        }
        Ok(())
    }
}
