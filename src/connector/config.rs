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
    Es(EsConnectorConfig),
}

impl ConnectorConfig {
    /// The operation gates for connectors that carry them (db, es).
    pub fn operation_gates(&self) -> Option<&OperationGates> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.operations),
            ConnectorConfig::Es(c) => Some(&c.operations),
            _ => None,
        }
    }
}

/// Per-connector operation gates. Everything defaults to allowed; disabling an
/// operation turns the corresponding handler call into a located validation
/// error, so a connector can be made read-only (or insert-only, …) in its
/// config without touching workflows.
///
/// - `read` gates `data_query`, `db_read`, and `mongo_read`.
/// - `insert` / `update` / `delete` / `upsert` gate the matching `data_write` op.
/// - `raw_write` gates the raw-SQL `db_write` escape hatch, which cannot be
///   classified per-op (hand-written SQL may contain any statement).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationGates {
    pub read: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub upsert: bool,
    pub raw_write: bool,
}

impl Default for OperationGates {
    fn default() -> Self {
        Self {
            read: true,
            insert: true,
            update: true,
            delete: true,
            upsert: true,
            raw_write: true,
        }
    }
}

impl OperationGates {
    /// Whether the named operation is enabled. Unknown names are denied
    /// (defensive — callers pass the fixed set above).
    pub fn allows(&self, op: &str) -> bool {
        match op {
            "read" => self.read,
            "insert" => self.insert,
            "update" => self.update,
            "delete" => self.delete,
            "upsert" => self.upsert,
            "raw_write" => self.raw_write,
            _ => false,
        }
    }
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
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: OperationGates,
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

/// Elasticsearch connector: a REST endpoint queried by the `data_query` handler
/// (executed via the shared HTTP client — no dedicated ES driver).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EsConnectorConfig {
    /// Base URL, e.g. `http://localhost:9200`.
    pub url: String,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Allow requests to private/internal IP addresses. Default false (SSRF protection).
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Maximum response body size in bytes (F12), same default as the HTTP
    /// connector — an unbounded ES response was the one egress read left
    /// without a cap.
    #[serde(default = "default_max_response_size")]
    pub max_response_size: usize,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: OperationGates,
}

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] = &["http", "kafka", "db", "cache", "storage", "es"];

/// Allowed cache backend values.
pub const VALID_CACHE_BACKENDS: &[&str] = &["redis", "memory"];

/// Typed enum for the connector `type` field on create/update requests.
/// Wire format is lowercase ("http", "kafka", "db", "cache", "storage");
/// deserialization is case-insensitive so "HTTP" or "Kafka" also parse —
/// strictly additive on v0.1's accepted set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Http,
    Kafka,
    Db,
    Cache,
    Storage,
    Es,
}

impl ConnectorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Kafka => "kafka",
            Self::Db => "db",
            Self::Cache => "cache",
            Self::Storage => "storage",
            Self::Es => "es",
        }
    }
}

impl std::fmt::Display for ConnectorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ConnectorType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "kafka" => Ok(Self::Kafka),
            "db" => Ok(Self::Db),
            "cache" => Ok(Self::Cache),
            "storage" => Ok(Self::Storage),
            "es" => Ok(Self::Es),
            other => Err(serde::de::Error::unknown_variant(
                other,
                VALID_CONNECTOR_TYPES,
            )),
        }
    }
}

#[cfg(test)]
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
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Http(http) => {
                assert_eq!(http.url, "https://api.example.com");
                assert_eq!(http.retry.max_retries, 2);
                assert_eq!(http.retry.retry_delay_ms, 500);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => unreachable!("Expected Http config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_kafka() {
        let json = r#"{"type":"kafka","brokers":["localhost:9092"],"topic":"test-topic","group_id":"test-group"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Kafka(kafka) => {
                assert_eq!(kafka.brokers, vec!["localhost:9092"]);
                assert_eq!(kafka.topic, "test-topic");
                assert_eq!(kafka.group_id, Some("test-group".to_string()));
            }
            _ => unreachable!("Expected Kafka config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_db() {
        let json = r#"{"type":"db","connection_string":"postgres://localhost/mydb","driver":"postgres","max_connections":5}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Db(db) => {
                assert_eq!(db.connection_string, "postgres://localhost/mydb");
                assert_eq!(db.driver, "postgres");
                assert_eq!(db.max_connections, Some(5));
            }
            _ => unreachable!("Expected Db config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_redis() {
        let json = r#"{"type":"cache","backend":"redis","url":"redis://localhost:6379","default_ttl_secs":300}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "redis");
                assert_eq!(cache.url, Some("redis://localhost:6379".to_string()));
                assert_eq!(cache.default_ttl_secs, Some(300));
            }
            _ => unreachable!("Expected Cache config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_memory() {
        let json = r#"{"type":"cache","backend":"memory","default_ttl_secs":60}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "memory");
                assert!(cache.url.is_none());
                assert_eq!(cache.default_ttl_secs, Some(60));
            }
            _ => unreachable!("Expected Cache config"),
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
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Storage(storage) => {
                assert_eq!(storage.provider, "s3");
                assert_eq!(storage.bucket, Some("my-bucket".to_string()));
                assert_eq!(storage.region, Some("us-east-1".to_string()));
            }
            _ => unreachable!("Expected Storage config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_es() {
        let json = r#"{"type":"es","url":"http://localhost:9200"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Es(es) => {
                assert_eq!(es.url, "http://localhost:9200");
                assert!(es.auth.is_none());
                assert!(!es.allow_private_urls);
            }
            _ => unreachable!("Expected Es config"),
        }
    }

    #[test]
    fn test_valid_connector_types_expanded() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"kafka"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"db"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"cache"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"storage"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"es"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
    }

    #[test]
    fn test_operation_gates_default_all_allowed() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("db has gates");
        for op in ["read", "insert", "update", "delete", "upsert", "raw_write"] {
            assert!(gates.allows(op), "{op} should default to allowed");
        }
        assert!(!gates.allows("unknown"), "unknown ops are denied");
    }

    #[test]
    fn test_operation_gates_partial_override() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:",
            "operations":{"delete":false,"raw_write":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("db has gates");
        assert!(!gates.allows("delete"));
        assert!(!gates.allows("raw_write"));
        assert!(gates.allows("read"));
        assert!(gates.allows("insert"));
        assert!(gates.allows("update"));
        assert!(gates.allows("upsert"));
    }

    #[test]
    fn test_operation_gates_on_es() {
        let json = r#"{"type":"es","url":"http://localhost:9200","operations":{"update":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("es has gates");
        assert!(!gates.allows("update"));
        assert!(gates.allows("insert"));
    }

    #[test]
    fn test_operation_gates_absent_on_http() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        assert!(config.operation_gates().is_none());
    }

    #[test]
    fn test_http_connector_config_defaults() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Http(http) => {
                assert!(http.headers.is_empty());
                assert!(http.auth.is_none());
                assert_eq!(http.retry.max_retries, 3);
                assert_eq!(http.retry.retry_delay_ms, 1000);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => unreachable!("Expected Http config"),
        }
    }
}
