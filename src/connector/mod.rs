use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::errors::OrionError;
use crate::storage::repositories::connectors::ConnectorRepository;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Http,
    Kafka,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorConfig {
    Http(HttpConnectorConfig),
    Kafka(KafkaConnectorConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConnectorConfig {
    pub url: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Bearer { token: String },
    Basic { username: String, password: String },
    ApiKey { header: String, key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay_ms() -> u64 {
    1000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            retry_delay_ms: default_retry_delay_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaConnectorConfig {
    pub brokers: Vec<String>,
    pub topic: String,
    pub group_id: Option<String>,
}

/// In-memory registry for active connector configurations.
pub struct ConnectorRegistry {
    configs: RwLock<HashMap<String, Arc<ConnectorConfig>>>,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
        }
    }

    /// Load all enabled connectors from the repository into the registry.
    pub async fn load_from_repo(
        &self,
        repo: &dyn ConnectorRepository,
    ) -> Result<usize, OrionError> {
        let connectors = repo.list_enabled().await?;
        let mut configs = self.configs.write().await;
        configs.clear();

        for connector in &connectors {
            if let Ok(config) = serde_json::from_str::<ConnectorConfig>(&connector.config_json) {
                configs.insert(connector.name.clone(), Arc::new(config));
            } else {
                tracing::warn!(
                    connector_id = %connector.id,
                    connector_name = %connector.name,
                    "Failed to parse connector config, skipping"
                );
            }
        }

        Ok(configs.len())
    }

    /// Get a connector config by name.
    pub async fn get(&self, name: &str) -> Option<Arc<ConnectorConfig>> {
        self.configs.read().await.get(name).cloned()
    }

    /// Reload all connectors from the repository.
    pub async fn reload(&self, repo: &dyn ConnectorRepository) -> Result<usize, OrionError> {
        self.load_from_repo(repo).await
    }
}
