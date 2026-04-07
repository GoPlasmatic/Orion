//! Dynamic SQL connection pool cache for external database connectors.
//!
//! Lazily creates and caches [`sqlx::AnyPool`] connections keyed by connector
//! name. The pool backend (Postgres, MySQL, SQLite) is selected at runtime
//! based on the connection-string URL scheme.
//!
//! **Important:** [`sqlx::any::install_default_drivers()`] must be called once
//! at application startup before any pool is created.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::{AnyPool, any::AnyPoolOptions};
use tokio::sync::RwLock;

use crate::connector::DbConnectorConfig;
use crate::connector::POOL_ACCESS_COUNTER;
use crate::errors::OrionError;

/// A cached pool with LRU tracking.
struct PoolEntry {
    pool: AnyPool,
    last_access: AtomicU64,
}

impl PoolEntry {
    fn new(pool: AnyPool) -> Self {
        Self {
            pool,
            last_access: AtomicU64::new(POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn touch(&self) {
        self.last_access
            .store(POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Lazily creates and caches SQL connection pools keyed by connector name.
/// Bounded by `max_entries` with LRU eviction.
pub struct SqlPoolCache {
    pools: RwLock<HashMap<String, PoolEntry>>,
    max_entries: usize,
}

impl SqlPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
            max_entries,
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
            if let Some(entry) = pools.get(connector_name) {
                entry.touch();
                return Ok(entry.pool.clone());
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
            existing.touch();
            return Ok(existing.pool.clone());
        }

        // LRU eviction when at capacity
        if pools.len() >= self.max_entries {
            if let Some(lru_key) = pools
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                tracing::info!(
                    evicted = %lru_key,
                    cache = "sql_pool",
                    "Pool cache at capacity, evicting least-recently-used entry"
                );
                pools.remove(&lru_key);
            }
        }

        pools.insert(connector_name.to_string(), PoolEntry::new(pool.clone()));
        Ok(pool)
    }

    /// Evict a cached pool (e.g., when connector config changes).
    pub async fn evict(&self, connector_name: &str) {
        self.pools.write().await.remove(connector_name);
    }
}

impl Default for SqlPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
