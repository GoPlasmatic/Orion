use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use redis::aio::MultiplexedConnection;
use tokio::sync::RwLock;

use crate::connector::CacheConnectorConfig;
use crate::connector::POOL_ACCESS_COUNTER;
use crate::errors::OrionError;

struct ConnEntry {
    conn: MultiplexedConnection,
    last_access: AtomicU64,
}

impl ConnEntry {
    fn new(conn: MultiplexedConnection) -> Self {
        Self {
            conn,
            last_access: AtomicU64::new(POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn touch(&self) {
        self.last_access.store(
            POOL_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

pub struct RedisPoolCache {
    connections: RwLock<HashMap<String, ConnEntry>>,
    max_entries: usize,
}

impl RedisPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            max_entries,
        }
    }

    pub async fn get_conn(
        &self,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<MultiplexedConnection, OrionError> {
        {
            let conns = self.connections.read().await;
            if let Some(entry) = conns.get(connector_name) {
                entry.touch();
                return Ok(entry.conn.clone());
            }
        }
        let mut conns = self.connections.write().await;
        if let Some(entry) = conns.get(connector_name) {
            entry.touch();
            return Ok(entry.conn.clone());
        }

        let url = config.url.as_deref().ok_or_else(|| {
            OrionError::BadRequest(format!(
                "Cache connector '{}' with backend='redis' requires a 'url'",
                connector_name
            ))
        })?;
        let client = redis::Client::open(url).map_err(|e| OrionError::InternalSource {
            context: format!("Invalid Redis URL for '{}'", connector_name),
            source: Box::new(e),
        })?;
        let conn = client
            .get_multiplexed_tokio_connection()
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Failed to connect to Redis '{}'", connector_name),
                source: Box::new(e),
            })?;

        // LRU eviction when at capacity
        if conns.len() >= self.max_entries {
            if let Some(lru_key) = conns
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                tracing::info!(
                    evicted = %lru_key,
                    cache = "redis_pool",
                    "Pool cache at capacity, evicting least-recently-used entry"
                );
                conns.remove(&lru_key);
            }
        }

        conns.insert(connector_name.to_string(), ConnEntry::new(conn.clone()));
        Ok(conn)
    }

    pub async fn evict(&self, connector_name: &str) {
        self.connections.write().await.remove(connector_name);
    }
}

impl Default for RedisPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
