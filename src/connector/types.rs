use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

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
