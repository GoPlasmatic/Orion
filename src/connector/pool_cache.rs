//! Dynamic SQL connection pool cache for external database connectors.
//!
//! Lazily creates and caches [`sqlx::AnyPool`] connections keyed by connector
//! name. The pool backend (Postgres, MySQL, SQLite) is selected at runtime
//! based on the connection-string URL scheme.
//!
//! **Important:** [`sqlx::any::install_default_drivers()`] must be called once
//! at application startup before any pool is created.

use std::time::Duration;

use sqlx::{AnyPool, any::AnyPoolOptions};

use super::lru_cache::LruCache;
use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

/// Lazily creates and caches SQL connection pools keyed by connector name.
/// Bounded by `max_entries` with LRU eviction.
pub struct SqlPoolCache {
    cache: LruCache<AnyPool>,
}

impl SqlPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: LruCache::new(max_entries, "sql_pool"),
        }
    }

    /// Get or lazily create a pool for the named connector.
    pub async fn get_pool(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<AnyPool, OrionError> {
        let conn_str = config.connection_string.clone();
        let max_conns = config.max_connections.unwrap_or(5);
        let connect_timeout = config.connect_timeout_ms.unwrap_or(5000);

        self.cache
            .get_or_create(connector_name, || async move {
                AnyPoolOptions::new()
                    .max_connections(max_conns)
                    .acquire_timeout(Duration::from_millis(connect_timeout))
                    .connect(&conn_str)
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!("Failed to connect to external DB '{connector_name}'"),
                        source: Box::new(e),
                    })
            })
            .await
    }

    /// Evict a cached pool (e.g., when connector config changes).
    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }
}

impl Default for SqlPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
