use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Run pending migrations automatically at startup. Set `false` in
    /// multi-replica deployments so boot-racing replicas never migrate;
    /// run `orion-server migrate` as a deploy step instead. Startup fails
    /// hard when this is `false` and migrations are pending.
    pub auto_migrate: bool,
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
            auto_migrate: true,
        }
    }
}

impl StorageConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonempty(&self.url, "storage.url")?;
        require_nonzero(self.busy_timeout_ms, "storage.busy_timeout_ms")?;
        require_nonzero(self.acquire_timeout_secs, "storage.acquire_timeout_secs")?;
        Ok(())
    }
}
