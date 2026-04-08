use serde::{Deserialize, Serialize};

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
        }
    }
}
