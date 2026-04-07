
use std::collections::HashMap;

use redis::aio::MultiplexedConnection;
use tokio::sync::RwLock;

use crate::connector::CacheConnectorConfig;
use crate::errors::OrionError;

pub struct RedisPoolCache {
    connections: RwLock<HashMap<String, MultiplexedConnection>>,
}

impl RedisPoolCache {
    pub fn new() -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_conn(
        &self,
        connector_name: &str,
        config: &CacheConnectorConfig,
    ) -> Result<MultiplexedConnection, OrionError> {
        {
            let conns = self.connections.read().await;
            if let Some(conn) = conns.get(connector_name) {
                return Ok(conn.clone());
            }
        }
        let mut conns = self.connections.write().await;
        if let Some(conn) = conns.get(connector_name) {
            return Ok(conn.clone());
        }

        let client = redis::Client::open(config.url.as_str()).map_err(|e| {
            OrionError::InternalSource {
                context: format!("Invalid Redis URL for '{}'", connector_name),
                source: Box::new(e),
            }
        })?;
        let conn = client
            .get_multiplexed_tokio_connection()
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Failed to connect to Redis '{}'", connector_name),
                source: Box::new(e),
            })?;
        conns.insert(connector_name.to_string(), conn.clone());
        Ok(conn)
    }

    pub async fn evict(&self, connector_name: &str) {
        self.connections.write().await.remove(connector_name);
    }
}

impl Default for RedisPoolCache {
    fn default() -> Self {
        Self::new()
    }
}
