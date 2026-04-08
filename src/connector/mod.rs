pub mod cache_backend;
pub mod circuit_breaker;
#[cfg(feature = "connectors-mongodb")]
pub mod mongo_pool;
#[cfg(feature = "connectors-sql")]
pub mod pool_cache;
#[cfg(feature = "connectors-redis")]
pub mod redis_pool;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

use crate::errors::OrionError;
use crate::storage::repositories::connectors::ConnectorRepository;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

/// Monotonic counter for LRU tracking of circuit breaker access.
static BREAKER_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Shared monotonic counter for LRU tracking across connector pool caches.
#[cfg(any(
    feature = "connectors-sql",
    feature = "connectors-redis",
    feature = "connectors-mongodb"
))]
pub(crate) static POOL_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A circuit breaker entry with LRU tracking.
struct BreakerEntry {
    breaker: Arc<CircuitBreaker>,
    last_access: AtomicU64,
}

impl BreakerEntry {
    fn new(breaker: Arc<CircuitBreaker>) -> Self {
        Self {
            breaker,
            last_access: AtomicU64::new(BREAKER_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn touch(&self) {
        self.last_access.store(
            BREAKER_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorConfig {
    Http(HttpConnectorConfig),
    Kafka(KafkaConnectorConfig),
    Db(DbConnectorConfig),
    Cache(CacheConnectorConfig),
    Storage(StorageConnectorConfig),
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
    /// Allow requests to private/internal IP addresses. Default false (SSRF protection).
    #[serde(default)]
    pub allow_private_urls: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConnectorConfig {
    pub connection_string: String,
    #[serde(default = "default_db_driver")]
    pub driver: String,
    #[serde(default)]
    pub max_connections: Option<u32>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub query_timeout_ms: Option<u64>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_db_driver() -> String {
    "postgres".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConnectorConfig {
    /// Cache backend: `"redis"` or `"memory"`. Required — no default.
    pub backend: String,
    /// Connection URL. Required when `backend = "redis"`, ignored for `"memory"`.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub default_ttl_secs: Option<u64>,
    #[serde(default)]
    pub max_connections: Option<u32>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConnectorConfig {
    pub provider: String,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub base_path: Option<String>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
}

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] = &["http", "kafka", "db", "cache", "storage"];

/// Allowed cache backend values.
pub const VALID_CACHE_BACKENDS: &[&str] = &["redis", "memory"];

/// In-memory registry for active connector configurations.
pub struct ConnectorRegistry {
    configs: RwLock<HashMap<String, Arc<ConnectorConfig>>>,
    circuit_breakers: RwLock<HashMap<String, BreakerEntry>>,
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
            if let Some(entry) = breakers.get(key) {
                entry.touch();
                return entry.breaker.clone();
            }
        }
        // Slow path: write lock on miss
        let mut breakers = self.circuit_breakers.write().await;
        // Double-check after acquiring write lock
        if let Some(entry) = breakers.get(key) {
            entry.touch();
            return entry.breaker.clone();
        }

        // Evict LRU entry if at capacity
        let max = self.cb_config.max_breakers;
        if breakers.len() >= max {
            if breakers.len() >= max * 9 / 10 {
                tracing::warn!(
                    count = breakers.len(),
                    max = max,
                    "Circuit breaker map approaching capacity limit"
                );
            }
            // Find the least-recently-used entry
            if let Some(lru_key) = breakers
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                breakers.remove(&lru_key);
            }
        }

        let breaker = Arc::new(CircuitBreaker::new(self.cb_config.clone()));
        let entry = BreakerEntry::new(breaker.clone());
        breakers.insert(key.to_string(), entry);
        breaker
    }

    /// Return all circuit breaker states for admin/health introspection.
    pub async fn circuit_breaker_states(&self) -> HashMap<String, String> {
        let breakers = self.circuit_breakers.read().await;
        breakers
            .iter()
            .map(|(k, v)| (k.clone(), v.breaker.state_name().to_string()))
            .collect()
    }

    /// Force-reset a circuit breaker by key. Returns `true` if the key existed.
    pub async fn reset_circuit_breaker(&self, key: &str) -> bool {
        let breakers = self.circuit_breakers.read().await;
        if let Some(entry) = breakers.get(key) {
            entry.breaker.reset();
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
const TOP_LEVEL_SECRET_KEYS: &[&str] = &[
    "password",
    "secret",
    "api_key",
    "token",
    "connection_string",
];

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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    #[tokio::test]
    async fn test_circuit_breaker_bounded_capacity() {
        let config = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            max_breakers: 3,
        };
        let registry = ConnectorRegistry::new(config);

        // Fill to capacity
        let _b1 = registry.get_or_create_breaker("key1").await;
        let _b2 = registry.get_or_create_breaker("key2").await;
        let _b3 = registry.get_or_create_breaker("key3").await;

        // Access key2 and key3 to make key1 the LRU
        let _b2_again = registry.get_or_create_breaker("key2").await;
        let _b3_again = registry.get_or_create_breaker("key3").await;

        // Adding a 4th should evict key1 (LRU)
        let _b4 = registry.get_or_create_breaker("key4").await;

        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.len(), 3);
        assert!(
            !states.contains_key("key1"),
            "key1 should have been evicted as LRU"
        );
        assert!(states.contains_key("key2"));
        assert!(states.contains_key("key3"));
        assert!(states.contains_key("key4"));
    }

    #[test]
    fn test_connector_config_deserialization_db() {
        let json = r#"{"type":"db","connection_string":"postgres://localhost/mydb","driver":"postgres","max_connections":5}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Db(db) => {
                assert_eq!(db.connection_string, "postgres://localhost/mydb");
                assert_eq!(db.driver, "postgres");
                assert_eq!(db.max_connections, Some(5));
            }
            _ => panic!("Expected Db config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_redis() {
        let json = r#"{"type":"cache","backend":"redis","url":"redis://localhost:6379","default_ttl_secs":300}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "redis");
                assert_eq!(cache.url, Some("redis://localhost:6379".to_string()));
                assert_eq!(cache.default_ttl_secs, Some(300));
            }
            _ => panic!("Expected Cache config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_memory() {
        let json = r#"{"type":"cache","backend":"memory","default_ttl_secs":60}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "memory");
                assert!(cache.url.is_none());
                assert_eq!(cache.default_ttl_secs, Some(60));
            }
            _ => panic!("Expected Cache config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_missing_backend() {
        // backend is required — deserialization should fail
        let json = r#"{"type":"cache","url":"redis://localhost:6379"}"#;
        let result = serde_json::from_str::<ConnectorConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_connector_config_deserialization_storage() {
        let json =
            r#"{"type":"storage","provider":"s3","bucket":"my-bucket","region":"us-east-1"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).unwrap();
        match config {
            ConnectorConfig::Storage(storage) => {
                assert_eq!(storage.provider, "s3");
                assert_eq!(storage.bucket, Some("my-bucket".to_string()));
                assert_eq!(storage.region, Some("us-east-1".to_string()));
            }
            _ => panic!("Expected Storage config"),
        }
    }

    #[test]
    fn test_mask_connector_secrets_connection_string() {
        let config = r#"{"type":"db","connection_string":"postgres://user:pass@host/db","driver":"postgres"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["connection_string"], "******");
        assert_eq!(val["driver"], "postgres");
    }

    #[test]
    fn test_valid_connector_types_expanded() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"kafka"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"db"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"cache"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"storage"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
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
