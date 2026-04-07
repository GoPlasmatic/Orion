use std::collections::HashMap;

use mongodb::Client;
use tokio::sync::RwLock;

use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

pub struct MongoPoolCache {
    clients: RwLock<HashMap<String, Client>>,
}

impl MongoPoolCache {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_client(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<Client, OrionError> {
        // Read lock fast path
        {
            let clients = self.clients.read().await;
            if let Some(client) = clients.get(connector_name) {
                return Ok(client.clone());
            }
        }
        // Write lock slow path
        let mut clients = self.clients.write().await;
        if let Some(client) = clients.get(connector_name) {
            return Ok(client.clone());
        }

        let client = Client::with_uri_str(&config.connection_string)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Failed to connect to MongoDB '{}'", connector_name),
                source: Box::new(e),
            })?;
        clients.insert(connector_name.to_string(), client.clone());
        Ok(client)
    }

    pub async fn evict(&self, connector_name: &str) {
        self.clients.write().await.remove(connector_name);
    }
}

impl Default for MongoPoolCache {
    fn default() -> Self {
        Self::new()
    }
}
