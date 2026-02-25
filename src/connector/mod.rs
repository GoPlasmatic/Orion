pub mod circuit_breaker;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::errors::OrionError;
use crate::storage::repositories::connectors::ConnectorRepository;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

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
    /// Maximum response body size in bytes (default 10 MB). Prevents OOM from large responses.
    #[serde(default = "default_max_response_size")]
    pub max_response_size: usize,
}

fn default_max_response_size() -> usize {
    10 * 1024 * 1024 // 10 MB
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

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] = &["http", "kafka"];

/// In-memory registry for active connector configurations.
pub struct ConnectorRegistry {
    configs: RwLock<HashMap<String, Arc<ConnectorConfig>>>,
    circuit_breakers: RwLock<HashMap<String, Arc<CircuitBreaker>>>,
    cb_config: CircuitBreakerConfig,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl ConnectorRegistry {
    pub fn new(cb_config: CircuitBreakerConfig) -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
            circuit_breakers: RwLock::new(HashMap::new()),
            cb_config,
        }
    }

    /// Get or create a circuit breaker for the given key (e.g. "channel:connector").
    pub async fn get_or_create_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // Fast path: read lock
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(cb) = breakers.get(key) {
                return cb.clone();
            }
        }
        // Slow path: write lock on miss
        let mut breakers = self.circuit_breakers.write().await;
        breakers
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(self.cb_config.clone())))
            .clone()
    }

    /// Return all circuit breaker states for admin/health introspection.
    pub async fn circuit_breaker_states(&self) -> HashMap<String, String> {
        let breakers = self.circuit_breakers.read().await;
        breakers
            .iter()
            .map(|(k, v)| (k.clone(), v.state_name().to_string()))
            .collect()
    }

    /// Force-reset a circuit breaker by key. Returns `true` if the key existed.
    pub async fn reset_circuit_breaker(&self, key: &str) -> bool {
        let breakers = self.circuit_breakers.read().await;
        if let Some(cb) = breakers.get(key) {
            cb.reset();
            true
        } else {
            false
        }
    }

    /// Whether circuit breakers are enabled.
    pub fn circuit_breaker_enabled(&self) -> bool {
        self.cb_config.enabled
    }

    /// Load all enabled connectors from the repository into the registry.
    pub async fn load_from_repo(
        &self,
        repo: &dyn ConnectorRepository,
    ) -> Result<usize, OrionError> {
        let connectors = repo.list_enabled().await?;

        // Build new map outside the lock to avoid holding it during deserialization
        let mut new_configs = HashMap::new();
        for connector in &connectors {
            match serde_json::from_str::<ConnectorConfig>(&connector.config_json) {
                Ok(config) => {
                    new_configs.insert(connector.name.clone(), Arc::new(config));
                }
                Err(e) => {
                    tracing::warn!(
                        connector_id = %connector.id,
                        connector_name = %connector.name,
                        error = %e,
                        "Failed to parse connector config, skipping"
                    );
                }
            }
        }

        // Minimal write lock — just swap
        let count = new_configs.len();
        *self.configs.write().await = new_configs;
        Ok(count)
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

const MASK: &str = "******";

/// Mask sensitive fields in a connector's config_json for API responses.
pub fn mask_connector_secrets(config_json: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(config_json) else {
        return config_json.to_string();
    };

    if let Some(obj) = val.as_object_mut() {
        // Mask auth fields
        if let Some(auth) = obj.get_mut("auth")
            && let Some(auth_obj) = auth.as_object_mut()
        {
            for key in ["token", "password", "key", "secret"] {
                if auth_obj.contains_key(key) {
                    auth_obj.insert(key.to_string(), serde_json::Value::String(MASK.to_string()));
                }
            }
        }
        // Mask top-level secret-looking fields
        for key in ["password", "secret", "api_key", "token"] {
            if obj.contains_key(key) {
                obj.insert(key.to_string(), serde_json::Value::String(MASK.to_string()));
            }
        }
    }

    serde_json::to_string(&val).unwrap_or_else(|_| config_json.to_string())
}

/// Return a connector model with secrets masked.
pub fn mask_connector(
    connector: &crate::storage::models::Connector,
) -> crate::storage::models::Connector {
    let mut masked = connector.clone();
    masked.config_json = mask_connector_secrets(&masked.config_json);
    masked
}
