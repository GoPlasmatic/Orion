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
    /// Encrypt `connector_oauth_state.state_json` — the OAuth2 tokens a
    /// connection obtains — at rest (H3). Empty (the default) stores it as
    /// plaintext JSON. Set to the 64-hex-char encoding of a 32-byte key
    /// (`openssl rand -hex 32`) and every write is AES-256-GCM-encrypted
    /// before it reaches the database; reads
    /// decrypt transparently, and pre-existing plaintext rows keep loading
    /// until their next write re-encrypts them. Prefer the environment form
    /// (`ORION_STORAGE__CONNECTOR_ENCRYPTION_KEY`) over the config file — a
    /// key in the file sits beside the database it protects.
    pub connector_encryption_key: String,
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
    /// How often, in seconds, the deployment store's artifact collector runs
    /// (`0` = never). Each pass marks unreferenced artifacts draining and
    /// deletes the bytes of those a previous pass marked, so an artifact
    /// leaves the database two passes after its last reference does. In a
    /// cluster one node holds the `deployment_gc` lease and collects.
    pub artifact_gc_interval_secs: u64,
    /// An artifact written or revived within this many seconds is never
    /// collected, judged on the database clock — the window between
    /// uploading a definition and applying the deployment that names it.
    pub artifact_gc_grace_secs: u64,
    /// How many of the newest deployment revisions the collector keeps whole,
    /// besides the head and any revision a node reports holding. An older
    /// revision is compacted: its artifacts become collectable and rolling
    /// back to it is refused. Must be >= 1.
    pub deployment_retain_revisions: u32,
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
            connector_encryption_key: String::new(),
            backup_dir: "./backups".to_string(),
            backup_retention_count: None,
            auto_migrate: true,
            connect_retry_secs: 60,
            artifact_gc_interval_secs: 3600,
            artifact_gc_grace_secs: 86_400,
            deployment_retain_revisions: 20,
        }
    }
}

impl StorageConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonempty(&self.url, "storage.url")?;
        // #311: the scheme check used to run only inside the cluster-mode
        // cross-check in `validation.rs`, where `&&` short-circuited it away
        // for every single-node deployment. `validate-config` therefore
        // blessed a URL the startup path refuses, which is the one thing that
        // command exists to prevent. The check belongs to `[storage]`, not to
        // cluster mode.
        detect_backend(&self.url)?;
        require_nonzero(self.busy_timeout_ms, "storage.busy_timeout_ms")?;
        require_nonzero(self.acquire_timeout_secs, "storage.acquire_timeout_secs")?;
        // 0 would compact the head's own predecessors away the moment they
        // stop being the head, leaving no revision to roll back to.
        require_nonzero(
            u64::from(self.deployment_retain_revisions),
            "storage.deployment_retain_revisions",
        )?;
        // 0 would delete the backup just written; "keep none" is not a
        // retention policy — leave the field unset to disable pruning.
        // H3: fail at startup, not at the first connector read/write.
        if !self.connector_encryption_key.is_empty() {
            crate::config_encryption::ConfigCipher::from_hex(&self.connector_encryption_key)?;
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    Sqlite,
    Postgres,
    Mysql,
}

impl std::fmt::Display for DbBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite => write!(f, "sqlite"),
            Self::Postgres => write!(f, "postgres"),
            Self::Mysql => write!(f, "mysql"),
        }
    }
}

/// Detect the database backend from a connection URL.
pub fn detect_backend(url: &str) -> Result<DbBackend, OrionError> {
    if url.starts_with("sqlite:") || url.starts_with("file:") {
        Ok(DbBackend::Sqlite)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(DbBackend::Postgres)
    } else if url.starts_with("mysql://") || url.starts_with("mariadb://") {
        Ok(DbBackend::Mysql)
    } else {
        Err(OrionError::Config {
            message: format!(
                "Unsupported database URL scheme: {url}. Expected sqlite:, postgres://, or mysql://"
            ),
        })
    }
}
