use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use mongodb::Client;
use tokio::sync::RwLock;

use crate::connector::DbConnectorConfig;
use crate::connector::POOL_ACCESS_COUNTER;
use crate::errors::OrionError;

struct ClientEntry {
    client: Client,
    last_access: AtomicU64,
}

impl ClientEntry {
    fn new(client: Client) -> Self {
        Self {
            client,
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

pub struct MongoPoolCache {
    clients: RwLock<HashMap<String, ClientEntry>>,
    max_entries: usize,
}

impl MongoPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            max_entries,
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
            if let Some(entry) = clients.get(connector_name) {
                entry.touch();
                return Ok(entry.client.clone());
            }
        }
        // Write lock slow path
        let mut clients = self.clients.write().await;
        if let Some(entry) = clients.get(connector_name) {
            entry.touch();
            return Ok(entry.client.clone());
        }

        let client = Client::with_uri_str(&config.connection_string)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: format!("Failed to connect to MongoDB '{}'", connector_name),
                source: Box::new(e),
            })?;

        // LRU eviction when at capacity
        if clients.len() >= self.max_entries {
            if let Some(lru_key) = clients
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                tracing::info!(
                    evicted = %lru_key,
                    cache = "mongo_pool",
                    "Pool cache at capacity, evicting least-recently-used entry"
                );
                clients.remove(&lru_key);
            }
        }

        clients.insert(connector_name.to_string(), ClientEntry::new(client.clone()));
        Ok(client)
    }

    pub async fn evict(&self, connector_name: &str) {
        self.clients.write().await.remove(connector_name);
    }
}

impl Default for MongoPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
