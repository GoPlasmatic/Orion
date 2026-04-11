use redis::aio::MultiplexedConnection;

use super::lru_cache::LruCache;
use crate::connector::CacheConnectorConfig;
use crate::errors::OrionError;

pub struct RedisPoolCache {
    cache: LruCache<MultiplexedConnection>,
}

impl RedisPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: LruCache::new(max_entries, "redis_pool"),
        }
    }

    pub async fn get_conn(
        &self,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<MultiplexedConnection, OrionError> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| {
                OrionError::BadRequest(format!(
                    "Cache connector '{}' with backend='redis' requires a 'url'",
                    connector_name
                ))
            })?
            .to_string();

        self.cache
            .get_or_create(connector_name, || async move {
                let client =
                    redis::Client::open(url.as_str()).map_err(|e| OrionError::InternalSource {
                        context: format!("Invalid Redis URL for '{}'", connector_name),
                        source: Box::new(e),
                    })?;
                client
                    .get_multiplexed_async_connection()
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!("Failed to connect to Redis '{}'", connector_name),
                        source: Box::new(e),
                    })
            })
            .await
    }

    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }
}

impl Default for RedisPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
