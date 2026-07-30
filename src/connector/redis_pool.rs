use redis::aio::ConnectionManager;

use super::lru_cache::LruCache;
use crate::connector::CacheConnectorConfig;
use crate::errors::OrionError;

pub struct RedisPoolCache {
    cache: LruCache<ConnectionManager>,
}

impl RedisPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            // F17: no evict handler — redis::aio::ConnectionManager exposes
            // no close API; dropping the last clone disconnects, which
            // eviction achieves once in-flight users finish.
            cache: LruCache::new(max_entries, "redis_pool"),
        }
    }

    pub async fn get_conn(
        &self,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<ConnectionManager, OrionError> {
        let url = config.url.as_deref().ok_or_else(|| {
            OrionError::BadRequest(format!(
                "Cache connector '{connector_name}' with backend='redis' requires a 'url'"
            ))
        })?;

        self.cache
            .get_or_create(connector_name, || async move {
                // S6: refuse a private/internal target before dialling (see the
                // note in `pool_cache.rs` on why this is create-path only).
                crate::validation::check_cache_endpoint(connector_name, config).await?;

                let client = redis::Client::open(url).map_err(|e| OrionError::InternalSource {
                    context: format!("Invalid Redis URL for '{connector_name}'"),
                    source: Box::new(e),
                })?;
                client
                    .get_connection_manager()
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!("Failed to connect to Redis '{connector_name}'"),
                        source: Box::new(e),
                    })
            })
            .await
    }

    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }

    pub async fn evict_all(&self) {
        self.cache.evict_all().await;
    }
}

impl Default for RedisPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
