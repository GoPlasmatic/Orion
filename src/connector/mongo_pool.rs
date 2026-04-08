use mongodb::Client;

use super::lru_cache::LruCache;
use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

pub struct MongoPoolCache {
    cache: LruCache<Client>,
}

impl MongoPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: LruCache::new(max_entries, "mongo_pool"),
        }
    }

    pub async fn get_client(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<Client, OrionError> {
        let conn_str = config.connection_string.clone();

        self.cache
            .get_or_create(connector_name, || async move {
                Client::with_uri_str(&conn_str)
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!("Failed to connect to MongoDB '{connector_name}'"),
                        source: Box::new(e),
                    })
            })
            .await
    }

    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }
}

impl Default for MongoPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
