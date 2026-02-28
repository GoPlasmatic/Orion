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

/// Keys to mask inside an `auth` sub-object.
const AUTH_SECRET_KEYS: &[&str] = &["token", "password", "key", "secret"];

/// Keys to mask at the top level of a connector config.
const TOP_LEVEL_SECRET_KEYS: &[&str] = &["password", "secret", "api_key", "token"];

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
            for key in AUTH_SECRET_KEYS {
                if auth_obj.contains_key(*key) {
                    auth_obj.insert(
                        (*key).to_string(),
                        serde_json::Value::String(MASK.to_string()),
                    );
                }
            }
        }
        // Mask top-level secret-looking fields
        for key in TOP_LEVEL_SECRET_KEYS {
            if obj.contains_key(*key) {
                obj.insert(
                    (*key).to_string(),
                    serde_json::Value::String(MASK.to_string()),
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_connector_secrets_bearer_token() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret123"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_basic_password() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"basic","username":"user","password":"secret"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["password"], "******");
        // Username should NOT be masked
        assert_eq!(val["auth"]["username"], "user");
    }

    #[test]
    fn test_mask_connector_secrets_api_key() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"apikey","key":"mysecretkey"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["key"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_top_level_fields() {
        let config = r#"{"type":"http","url":"https://api.example.com","password":"top_secret","api_key":"ak123","token":"tk456","secret":"shhh"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["password"], "******");
        assert_eq!(val["api_key"], "******");
        assert_eq!(val["token"], "******");
        assert_eq!(val["secret"], "******");
        // URL should not be masked
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_no_auth() {
        let config = r#"{"type":"http","url":"https://api.example.com"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_invalid_json() {
        let config = "not valid json";
        let masked = mask_connector_secrets(config);
        assert_eq!(masked, config);
    }

    #[test]
    fn test_mask_connector_model() {
        use chrono::NaiveDate;
        let connector = crate::storage::models::Connector {
            id: "c1".to_string(),
            name: "test".to_string(),
            connector_type: "http".to_string(),
            config_json: r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret"}}"#.to_string(),
            enabled: true,
            created_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        };
        let masked = mask_connector(&connector);
        assert_eq!(masked.id, "c1");
        let val: serde_json::Value = serde_json::from_str(&masked.config_json).unwrap();
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_valid_connector_types() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"kafka"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_delay_ms, 1000);
    }

    #[test]
    fn test_connector_config_deserialization_http() {
        let json = r#"{"type":"http","url":"https://api.example.com","headers":{},"retry":{"max_retries":2,"retry_delay_ms":500}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Http(http) => {
                assert_eq!(http.url, "https://api.example.com");
                assert_eq!(http.retry.max_retries, 2);
                assert_eq!(http.retry.retry_delay_ms, 500);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => panic!("Expected Http config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_kafka() {
        let json = r#"{"type":"kafka","brokers":["localhost:9092"],"topic":"test-topic","group_id":"test-group"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Kafka(kafka) => {
                assert_eq!(kafka.brokers, vec!["localhost:9092"]);
                assert_eq!(kafka.topic, "test-topic");
                assert_eq!(kafka.group_id, Some("test-group".to_string()));
            }
            _ => panic!("Expected Kafka config"),
        }
    }

    #[tokio::test]
    async fn test_connector_registry_get_and_set() {
        let registry = ConnectorRegistry::default();
        assert!(registry.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_disabled_by_default() {
        let registry = ConnectorRegistry::default();
        assert!(!registry.circuit_breaker_enabled());
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_enabled() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
        };
        let registry = ConnectorRegistry::new(config);
        assert!(registry.circuit_breaker_enabled());
    }

    #[tokio::test]
    async fn test_connector_registry_get_or_create_breaker() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
        };
        let registry = ConnectorRegistry::new(config);
        let b1 = registry.get_or_create_breaker("key1").await;
        let b2 = registry.get_or_create_breaker("key1").await;
        // Should return the same breaker
        assert!(Arc::ptr_eq(&b1, &b2));
    }

    #[tokio::test]
    async fn test_connector_registry_circuit_breaker_states() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
        };
        let registry = ConnectorRegistry::new(config);
        let _ = registry.get_or_create_breaker("key1").await;
        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 1);
        assert_eq!(states.get("key1").unwrap(), "closed");
    }

    #[tokio::test]
    async fn test_connector_registry_reset_circuit_breaker() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 1,
            recovery_timeout_secs: 300,
        };
        let registry = ConnectorRegistry::new(config);
        let breaker = registry.get_or_create_breaker("key1").await;
        breaker.record_failure(); // trips it
        assert!(!breaker.check()); // open

        let found = registry.reset_circuit_breaker("key1").await;
        assert!(found);
        assert!(breaker.check()); // closed again
    }

    #[tokio::test]
    async fn test_connector_registry_reset_nonexistent_breaker() {
        let registry = ConnectorRegistry::default();
        assert!(!registry.reset_circuit_breaker("nope").await);
    }

    #[test]
    fn test_http_connector_config_defaults() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Http(http) => {
                assert!(http.headers.is_empty());
                assert!(http.auth.is_none());
                assert_eq!(http.retry.max_retries, 3);
                assert_eq!(http.retry.retry_delay_ms, 1000);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => panic!("Expected Http config"),
        }
    }
}
