use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::errors::OrionError;

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ingest: IngestConfig,
    pub engine: EngineConfig,
    pub queue: QueueConfig,
    pub kafka: KafkaIngestConfig,
    pub logging: LoggingConfig,
    pub metrics: MetricsConfig,
    pub cors: CorsConfig,
    pub tracing: TracingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub path: String,
    pub max_connections: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: "orion.db".to_string(),
            max_connections: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    pub max_payload_size: usize,
    pub batch_size: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 1_048_576, // 1 MB
            batch_size: 100,
        }
    }
}

/// Engine configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub circuit_breaker: crate::connector::circuit_breaker::CircuitBreakerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueConfig {
    /// Maximum number of concurrent async job workers.
    pub workers: usize,
    /// Channel buffer size for pending jobs.
    pub buffer_size: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            buffer_size: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KafkaIngestConfig {
    /// Enable Kafka consumer ingestion.
    pub enabled: bool,
    /// Kafka broker addresses.
    pub brokers: Vec<String>,
    /// Consumer group ID.
    pub group_id: String,
    /// Topic-to-channel mappings.
    #[serde(default)]
    pub topics: Vec<TopicMapping>,
    /// Dead-letter queue configuration.
    pub dlq: DlqConfig,
}

impl Default for KafkaIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            brokers: vec!["localhost:9092".to_string()],
            group_id: "orion".to_string(),
            topics: vec![],
            dlq: DlqConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMapping {
    pub topic: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DlqConfig {
    /// Enable dead-letter queue for failed messages.
    pub enabled: bool,
    /// DLQ topic name.
    pub topic: String,
}

impl Default for DlqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            topic: "orion-dlq".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Allowed origins. Use `["*"]` (default) for permissive CORS.
    pub allowed_origins: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_string()],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TracingConfig {
    /// Enable OpenTelemetry trace export. Requires the `otel` feature flag at compile time.
    pub enabled: bool,
    /// OTLP gRPC endpoint (e.g. Jaeger, Grafana Tempo, OTel Collector).
    pub otlp_endpoint: String,
    /// Service name reported in traces.
    pub service_name: String,
    /// Sampling rate from 0.0 (none) to 1.0 (all).
    pub sample_rate: f64,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "orion".to_string(),
            sample_rate: 1.0,
        }
    }
}

/// Load configuration from an optional TOML file path, then apply env overrides.
pub fn load_config(path: Option<&str>) -> Result<AppConfig, OrionError> {
    let mut config = if let Some(p) = path {
        let content =
            std::fs::read_to_string(Path::new(p)).map_err(|e| OrionError::InternalSource {
                context: format!("Failed to read config file '{}'", p),
                source: Box::new(e),
            })?;
        toml::from_str::<AppConfig>(&content).map_err(|e| OrionError::InternalSource {
            context: format!("Failed to parse config file '{}'", p),
            source: Box::new(e),
        })?
    } else {
        AppConfig::default()
    };

    apply_env_overrides(&mut config);
    validate_config(&config)?;

    Ok(config)
}

/// Apply ORION_* environment variable overrides.
fn apply_env_overrides(config: &mut AppConfig) {
    apply_env_overrides_with(config, |key| std::env::var(key));
}

/// Testable version that accepts a custom env reader.
fn apply_env_overrides_with<F>(config: &mut AppConfig, env_var: F)
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    if let Ok(v) = env_var("ORION_SERVER__HOST") {
        config.server.host = v;
    }
    if let Ok(v) = env_var("ORION_SERVER__PORT")
        && let Ok(port) = v.parse::<u16>()
    {
        config.server.port = port;
    }
    if let Ok(v) = env_var("ORION_STORAGE__PATH") {
        config.storage.path = v;
    }
    if let Ok(v) = env_var("ORION_LOGGING__LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = env_var("ORION_LOGGING__FORMAT") {
        match v.to_lowercase().as_str() {
            "json" => config.logging.format = LogFormat::Json,
            "pretty" => config.logging.format = LogFormat::Pretty,
            _ => {}
        }
    }
    if let Ok(v) = env_var("ORION_INGEST__MAX_PAYLOAD_SIZE")
        && let Ok(size) = v.parse::<usize>()
    {
        config.ingest.max_payload_size = size;
    }
    if let Ok(v) = env_var("ORION_INGEST__BATCH_SIZE")
        && let Ok(size) = v.parse::<usize>()
    {
        config.ingest.batch_size = size;
    }
    if let Ok(v) = env_var("ORION_QUEUE__WORKERS")
        && let Ok(w) = v.parse::<usize>()
    {
        config.queue.workers = w;
    }
    if let Ok(v) = env_var("ORION_QUEUE__BUFFER_SIZE")
        && let Ok(s) = v.parse::<usize>()
    {
        config.queue.buffer_size = s;
    }
    if let Ok(v) = env_var("ORION_METRICS__ENABLED")
        && let Ok(enabled) = v.parse::<bool>()
    {
        config.metrics.enabled = enabled;
    }
    // Tracing overrides
    if let Ok(v) = env_var("ORION_TRACING__ENABLED")
        && let Ok(enabled) = v.parse::<bool>()
    {
        config.tracing.enabled = enabled;
    }
    if let Ok(v) = env_var("ORION_TRACING__OTLP_ENDPOINT") {
        config.tracing.otlp_endpoint = v;
    }
    if let Ok(v) = env_var("ORION_TRACING__SERVICE_NAME") {
        config.tracing.service_name = v;
    }
    if let Ok(v) = env_var("ORION_TRACING__SAMPLE_RATE")
        && let Ok(rate) = v.parse::<f64>()
    {
        config.tracing.sample_rate = rate;
    }
    // Circuit breaker overrides
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__ENABLED")
        && let Ok(enabled) = v.parse::<bool>()
    {
        config.engine.circuit_breaker.enabled = enabled;
    }
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD")
        && let Ok(t) = v.parse::<u32>()
    {
        config.engine.circuit_breaker.failure_threshold = t;
    }
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS")
        && let Ok(t) = v.parse::<u64>()
    {
        config.engine.circuit_breaker.recovery_timeout_secs = t;
    }
    // Kafka overrides
    if let Ok(v) = env_var("ORION_KAFKA__ENABLED")
        && let Ok(enabled) = v.parse::<bool>()
    {
        config.kafka.enabled = enabled;
    }
    if let Ok(v) = env_var("ORION_KAFKA__BROKERS") {
        config.kafka.brokers = v.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Ok(v) = env_var("ORION_KAFKA__GROUP_ID") {
        config.kafka.group_id = v;
    }
}

/// Valid tracing log levels.
const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// Validate configuration values.
fn validate_config(config: &AppConfig) -> Result<(), OrionError> {
    if config.server.port == 0 {
        return Err(OrionError::Config {
            message: "server.port must be > 0".to_string(),
        });
    }
    if config.ingest.max_payload_size == 0 {
        return Err(OrionError::Config {
            message: "ingest.max_payload_size must be > 0".to_string(),
        });
    }
    if config.ingest.batch_size == 0 {
        return Err(OrionError::Config {
            message: "ingest.batch_size must be > 0".to_string(),
        });
    }
    if config.queue.workers == 0 {
        return Err(OrionError::Config {
            message: "queue.workers must be > 0".to_string(),
        });
    }
    if config.queue.buffer_size == 0 {
        return Err(OrionError::Config {
            message: "queue.buffer_size must be > 0".to_string(),
        });
    }
    if config.storage.path.is_empty() {
        return Err(OrionError::Config {
            message: "storage.path must not be empty".to_string(),
        });
    }
    if !VALID_LOG_LEVELS.contains(&config.logging.level.to_lowercase().as_str()) {
        return Err(OrionError::Config {
            message: format!(
                "logging.level '{}' is invalid. Must be one of: {}",
                config.logging.level,
                VALID_LOG_LEVELS.join(", ")
            ),
        });
    }
    if config.tracing.enabled {
        if config.tracing.otlp_endpoint.is_empty() {
            return Err(OrionError::Config {
                message: "tracing.otlp_endpoint must not be empty when tracing is enabled"
                    .to_string(),
            });
        }
        if !(0.0..=1.0).contains(&config.tracing.sample_rate) {
            return Err(OrionError::Config {
                message: "tracing.sample_rate must be between 0.0 and 1.0".to_string(),
            });
        }
    }
    #[cfg(feature = "kafka")]
    if config.kafka.enabled {
        if config.kafka.brokers.is_empty() {
            return Err(OrionError::Config {
                message: "kafka.brokers must not be empty when Kafka is enabled".to_string(),
            });
        }
        if config.kafka.group_id.is_empty() {
            return Err(OrionError::Config {
                message: "kafka.group_id must not be empty when Kafka is enabled".to_string(),
            });
        }
        if config.kafka.topics.is_empty() {
            return Err(OrionError::Config {
                message: "kafka.topics must not be empty when Kafka is enabled".to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.storage.path, "orion.db");
        assert_eq!(config.storage.max_connections, 10);
    }

    #[test]
    fn test_load_config_no_file() {
        let config = load_config(None).unwrap();
        // Port may be overridden by env vars in parallel tests, just check it loaded
        assert!(config.server.port > 0);
        assert!(!config.server.host.is_empty());
    }

    #[test]
    fn test_validate_config_invalid_port() {
        let mut config = AppConfig::default();
        config.server.port = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_invalid_queue_workers() {
        let mut config = AppConfig::default();
        config.queue.workers = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_invalid_queue_buffer() {
        let mut config = AppConfig::default();
        config.queue.buffer_size = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_empty_storage_path() {
        let mut config = AppConfig::default();
        config.storage.path = String::new();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_invalid_log_level() {
        let mut config = AppConfig::default();
        config.logging.level = "invalid".to_string();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    #[cfg(feature = "kafka")]
    fn test_validate_config_kafka_enabled_no_brokers() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.brokers = vec![];
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_env_override() {
        use std::collections::HashMap;

        let mut env = HashMap::new();
        env.insert("ORION_SERVER__PORT", "9090");
        env.insert("ORION_STORAGE__PATH", "custom.db");
        env.insert("ORION_LOGGING__LEVEL", "debug");
        env.insert("ORION_METRICS__ENABLED", "true");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, |key| {
            env.get(key)
                .map(|v| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        });
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.storage.path, "custom.db");
        assert_eq!(config.logging.level, "debug");
        assert!(config.metrics.enabled);
    }

    #[test]
    fn test_toml_parsing() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 3000

[storage]
path = "test.db"

[logging]
level = "debug"
format = "json"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.storage.path, "test.db");
        assert_eq!(config.logging.level, "debug");
    }
}
