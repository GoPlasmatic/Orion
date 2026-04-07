//! Dynamic SQL connection pool cache for external database connectors.
//!
//! Lazily creates and caches [`sqlx::AnyPool`] connections keyed by connector
//! name. The pool backend (Postgres, MySQL, SQLite) is selected at runtime
//! based on the connection-string URL scheme.
//!
//! **Important:** [`sqlx::any::install_default_drivers()`] must be called once
//! at application startup before any pool is created.

use std::collections::HashMap;
use std::time::Duration;

use sqlx::{AnyPool, any::AnyPoolOptions};
use tokio::sync::RwLock;

use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

/// Lazily creates and caches SQL connection pools keyed by connector name.
pub struct SqlPoolCache {
    pools: RwLock<HashMap<String, AnyPool>>,
}

impl SqlPoolCache {
    pub fn new() -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
        }
    }

    /// Get or lazily create a pool for the named connector.
    pub async fn get_pool(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<AnyPool, OrionError> {
        // Fast path: read lock
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(connector_name) {
                return Ok(pool.clone());
            }
        }

        // Connect outside the write lock to avoid blocking other connectors
        let max_conns = config.max_connections.unwrap_or(5);
        let connect_timeout = config.connect_timeout_ms.unwrap_or(5000);

        let pool = AnyPoolOptions::new()
            .max_connections(max_conns)
            .acquire_timeout(Duration::from_millis(connect_timeout))
            .connect(&config.connection_string)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Failed to connect to external DB '{connector_name}'"),
                source: Box::new(e),
            })?;

        // Insert under write lock; if another task raced, use theirs
        let mut pools = self.pools.write().await;
        if let Some(existing) = pools.get(connector_name) {
            return Ok(existing.clone());
        }
        pools.insert(connector_name.to_string(), pool.clone());
        Ok(pool)
    }

    /// Evict a cached pool (e.g., when connector config changes).
    pub async fn evict(&self, connector_name: &str) {
        self.pools.write().await.remove(connector_name);
    }
}

impl Default for SqlPoolCache {
    fn default() -> Self {
        Self::new()
    }
}
