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
    pub rate_limit: RateLimitConfig,
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
    /// SQLite busy timeout in milliseconds.
    pub busy_timeout_ms: u64,
    /// Connection pool acquire timeout in seconds.
    pub acquire_timeout_secs: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: "orion.db".to_string(),
            max_connections: 10,
            busy_timeout_ms: 5000,
            acquire_timeout_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
    pub max_payload_size: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 1_048_576, // 1 MB
        }
    }
}

/// Engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub circuit_breaker: crate::connector::circuit_breaker::CircuitBreakerConfig,
    /// Timeout in seconds for acquiring engine read lock in health checks.
    pub health_check_timeout_secs: u64,
    /// Timeout in seconds for acquiring engine write lock during reload.
    pub reload_timeout_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            circuit_breaker: Default::default(),
            health_check_timeout_secs: 2,
            reload_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueConfig {
    /// Maximum number of concurrent async trace workers.
    pub workers: usize,
    /// Channel buffer size for pending traces.
    pub buffer_size: usize,
    /// Timeout in seconds to wait for in-flight traces during shutdown.
    pub shutdown_timeout_secs: u64,
    /// How long to retain completed/failed traces in hours (0 = forever).
    pub trace_retention_hours: u64,
    /// How often to run the trace cleanup task in seconds.
    pub trace_cleanup_interval_secs: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            buffer_size: 1000,
            shutdown_timeout_secs: 30,
            trace_retention_hours: 72,
            trace_cleanup_interval_secs: 3600,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    #[serde(default = "default_rps")]
    pub default_rps: u32,
    #[serde(default = "default_burst")]
    pub default_burst: u32,
    #[serde(default)]
    pub endpoints: EndpointRateLimits,
    #[serde(default)]
    pub channels: std::collections::HashMap<String, ChannelRateLimit>,
}

fn default_rps() -> u32 {
    100
}

fn default_burst() -> u32 {
    50
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointRateLimits {
    pub admin_rps: Option<u32>,
    pub data_rps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRateLimit {
    pub rps: u32,
    #[serde(default)]
    pub burst: Option<u32>,
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

    apply_env_overrides(&mut config)?;
    validate_config(&config)?;

    Ok(config)
}

/// Helper to parse an env var value, returning a clear error on failure.
fn parse_env<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, OrionError> {
    value.parse::<T>().map_err(|_| OrionError::Config {
        message: format!(
            "{}: invalid value '{}', expected {}",
            key,
            value,
            std::any::type_name::<T>()
        ),
    })
}

/// Apply ORION_* environment variable overrides.
fn apply_env_overrides(config: &mut AppConfig) -> Result<(), OrionError> {
    apply_env_overrides_with(config, |key| std::env::var(key))
}

/// Testable version that accepts a custom env reader.
fn apply_env_overrides_with<F>(config: &mut AppConfig, env_var: F) -> Result<(), OrionError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    if let Ok(v) = env_var("ORION_SERVER__HOST") {
        config.server.host = v;
    }
    if let Ok(v) = env_var("ORION_SERVER__PORT") {
        config.server.port = parse_env::<u16>("ORION_SERVER__PORT", &v)?;
    }
    if let Ok(v) = env_var("ORION_STORAGE__PATH") {
        config.storage.path = v;
    }
    if let Ok(v) = env_var("ORION_STORAGE__BUSY_TIMEOUT_MS") {
        config.storage.busy_timeout_ms = parse_env::<u64>("ORION_STORAGE__BUSY_TIMEOUT_MS", &v)?;
    }
    if let Ok(v) = env_var("ORION_STORAGE__ACQUIRE_TIMEOUT_SECS") {
        config.storage.acquire_timeout_secs =
            parse_env::<u64>("ORION_STORAGE__ACQUIRE_TIMEOUT_SECS", &v)?;
    }
    if let Ok(v) = env_var("ORION_LOGGING__LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = env_var("ORION_LOGGING__FORMAT") {
        match v.to_lowercase().as_str() {
            "json" => config.logging.format = LogFormat::Json,
            "pretty" => config.logging.format = LogFormat::Pretty,
            _ => {
                return Err(OrionError::Config {
                    message: format!(
                        "ORION_LOGGING__FORMAT: invalid value '{}', expected 'json' or 'pretty'",
                        v
                    ),
                });
            }
        }
    }
    if let Ok(v) = env_var("ORION_INGEST__MAX_PAYLOAD_SIZE") {
        config.ingest.max_payload_size = parse_env::<usize>("ORION_INGEST__MAX_PAYLOAD_SIZE", &v)?;
    }
    if let Ok(v) = env_var("ORION_QUEUE__WORKERS") {
        config.queue.workers = parse_env::<usize>("ORION_QUEUE__WORKERS", &v)?;
    }
    if let Ok(v) = env_var("ORION_QUEUE__BUFFER_SIZE") {
        config.queue.buffer_size = parse_env::<usize>("ORION_QUEUE__BUFFER_SIZE", &v)?;
    }
    if let Ok(v) = env_var("ORION_QUEUE__SHUTDOWN_TIMEOUT_SECS") {
        config.queue.shutdown_timeout_secs =
            parse_env::<u64>("ORION_QUEUE__SHUTDOWN_TIMEOUT_SECS", &v)?;
    }
    if let Ok(v) = env_var("ORION_QUEUE__TRACE_RETENTION_HOURS") {
        config.queue.trace_retention_hours =
            parse_env::<u64>("ORION_QUEUE__TRACE_RETENTION_HOURS", &v)?;
    }
    if let Ok(v) = env_var("ORION_QUEUE__TRACE_CLEANUP_INTERVAL_SECS") {
        config.queue.trace_cleanup_interval_secs =
            parse_env::<u64>("ORION_QUEUE__TRACE_CLEANUP_INTERVAL_SECS", &v)?;
    }
    if let Ok(v) = env_var("ORION_METRICS__ENABLED") {
        config.metrics.enabled = parse_env::<bool>("ORION_METRICS__ENABLED", &v)?;
    }
    // Tracing overrides
    if let Ok(v) = env_var("ORION_TRACING__ENABLED") {
        config.tracing.enabled = parse_env::<bool>("ORION_TRACING__ENABLED", &v)?;
    }
    if let Ok(v) = env_var("ORION_TRACING__OTLP_ENDPOINT") {
        config.tracing.otlp_endpoint = v;
    }
    if let Ok(v) = env_var("ORION_TRACING__SERVICE_NAME") {
        config.tracing.service_name = v;
    }
    if let Ok(v) = env_var("ORION_TRACING__SAMPLE_RATE") {
        config.tracing.sample_rate = parse_env::<f64>("ORION_TRACING__SAMPLE_RATE", &v)?;
    }
    // Engine overrides
    if let Ok(v) = env_var("ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS") {
        config.engine.health_check_timeout_secs =
            parse_env::<u64>("ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS", &v)?;
    }
    if let Ok(v) = env_var("ORION_ENGINE__RELOAD_TIMEOUT_SECS") {
        config.engine.reload_timeout_secs =
            parse_env::<u64>("ORION_ENGINE__RELOAD_TIMEOUT_SECS", &v)?;
    }
    // Circuit breaker overrides
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__ENABLED") {
        config.engine.circuit_breaker.enabled =
            parse_env::<bool>("ORION_ENGINE__CIRCUIT_BREAKER__ENABLED", &v)?;
    }
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD") {
        config.engine.circuit_breaker.failure_threshold =
            parse_env::<u32>("ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD", &v)?;
    }
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS") {
        config.engine.circuit_breaker.recovery_timeout_secs =
            parse_env::<u64>("ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS", &v)?;
    }
    if let Ok(v) = env_var("ORION_ENGINE__CIRCUIT_BREAKER__MAX_BREAKERS") {
        config.engine.circuit_breaker.max_breakers =
            parse_env::<usize>("ORION_ENGINE__CIRCUIT_BREAKER__MAX_BREAKERS", &v)?;
    }
    // Rate limit overrides
    if let Ok(v) = env_var("ORION_RATE_LIMIT__ENABLED") {
        config.rate_limit.enabled = parse_env::<bool>("ORION_RATE_LIMIT__ENABLED", &v)?;
    }
    if let Ok(v) = env_var("ORION_RATE_LIMIT__DEFAULT_RPS") {
        config.rate_limit.default_rps = parse_env::<u32>("ORION_RATE_LIMIT__DEFAULT_RPS", &v)?;
    }
    if let Ok(v) = env_var("ORION_RATE_LIMIT__DEFAULT_BURST") {
        config.rate_limit.default_burst = parse_env::<u32>("ORION_RATE_LIMIT__DEFAULT_BURST", &v)?;
    }
    // Kafka overrides
    if let Ok(v) = env_var("ORION_KAFKA__ENABLED") {
        config.kafka.enabled = parse_env::<bool>("ORION_KAFKA__ENABLED", &v)?;
    }
    if let Ok(v) = env_var("ORION_KAFKA__BROKERS") {
        config.kafka.brokers = v.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Ok(v) = env_var("ORION_KAFKA__GROUP_ID") {
        config.kafka.group_id = v;
    }
    Ok(())
}

/// Valid tracing log levels.
const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// Return a config error if `value` is zero.
fn require_nonzero(value: u64, field: &str) -> Result<(), OrionError> {
    if value == 0 {
        return Err(OrionError::Config {
            message: format!("{field} must be > 0"),
        });
    }
    Ok(())
}

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
    // Timeout validations
    require_nonzero(
        config.engine.health_check_timeout_secs,
        "engine.health_check_timeout_secs",
    )?;
    require_nonzero(
        config.engine.reload_timeout_secs,
        "engine.reload_timeout_secs",
    )?;
    require_nonzero(
        config.queue.shutdown_timeout_secs,
        "queue.shutdown_timeout_secs",
    )?;
    require_nonzero(config.storage.busy_timeout_ms, "storage.busy_timeout_ms")?;
    require_nonzero(
        config.storage.acquire_timeout_secs,
        "storage.acquire_timeout_secs",
    )?;
    if config.rate_limit.enabled {
        if config.rate_limit.default_rps == 0 {
            return Err(OrionError::Config {
                message: "rate_limit.default_rps must be > 0 when rate limiting is enabled"
                    .to_string(),
            });
        }
        if config.rate_limit.default_burst == 0 {
            return Err(OrionError::Config {
                message: "rate_limit.default_burst must be > 0 when rate limiting is enabled"
                    .to_string(),
            });
        }
    }
    // CORS warning
    if config.cors.allowed_origins.len() == 1 && config.cors.allowed_origins[0] == "*" {
        tracing::warn!(
            "CORS is set to permissive ('*'). For production, configure specific origins in [cors] allowed_origins"
        );
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
        let mut seen_topics = std::collections::HashSet::new();
        let mut seen_channels = std::collections::HashSet::new();
        for (i, mapping) in config.kafka.topics.iter().enumerate() {
            if mapping.topic.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.topics[{}].topic must not be empty", i),
                });
            }
            if mapping.channel.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.topics[{}].channel must not be empty", i),
                });
            }
            if !seen_topics.insert(&mapping.topic) {
                return Err(OrionError::Config {
                    message: format!("kafka.topics: duplicate topic '{}'", mapping.topic),
                });
            }
            if !seen_channels.insert(&mapping.channel) {
                return Err(OrionError::Config {
                    message: format!("kafka.topics: duplicate channel '{}'", mapping.channel),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.storage.path, "orion.db");
        assert_eq!(config.storage.max_connections, 10);
        assert_eq!(config.storage.busy_timeout_ms, 5000);
        assert_eq!(config.storage.acquire_timeout_secs, 5);
        assert_eq!(config.engine.health_check_timeout_secs, 2);
        assert_eq!(config.engine.reload_timeout_secs, 10);
        assert_eq!(config.queue.shutdown_timeout_secs, 30);
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
        })
        .unwrap();
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

    #[test]
    fn test_validate_config_valid_default() {
        let config = AppConfig::default();
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_invalid_max_payload_size() {
        let mut config = AppConfig::default();
        config.ingest.max_payload_size = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_tracing_enabled_empty_endpoint() {
        let mut config = AppConfig::default();
        config.tracing.enabled = true;
        config.tracing.otlp_endpoint = "".to_string();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_tracing_invalid_sample_rate() {
        let mut config = AppConfig::default();
        config.tracing.enabled = true;
        config.tracing.sample_rate = 1.5;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_tracing_negative_sample_rate() {
        let mut config = AppConfig::default();
        config.tracing.enabled = true;
        config.tracing.sample_rate = -0.1;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_rate_limit_zero_rps() {
        let mut config = AppConfig::default();
        config.rate_limit.enabled = true;
        config.rate_limit.default_rps = 0;
        config.rate_limit.default_burst = 10;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_rate_limit_zero_burst() {
        let mut config = AppConfig::default();
        config.rate_limit.enabled = true;
        config.rate_limit.default_rps = 100;
        config.rate_limit.default_burst = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_rate_limit_valid() {
        let mut config = AppConfig::default();
        config.rate_limit.enabled = true;
        config.rate_limit.default_rps = 100;
        config.rate_limit.default_burst = 50;
        assert!(validate_config(&config).is_ok());
    }

    fn make_env_reader<'a>(
        env: &'a HashMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> + 'a {
        move |key| {
            env.get(key)
                .map(|v: &&str| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    #[test]
    fn test_env_override_all_fields() {
        let mut env = HashMap::new();
        env.insert("ORION_SERVER__HOST", "localhost");
        env.insert("ORION_SERVER__PORT", "3000");
        env.insert("ORION_STORAGE__PATH", "test.db");
        env.insert("ORION_STORAGE__BUSY_TIMEOUT_MS", "10000");
        env.insert("ORION_STORAGE__ACQUIRE_TIMEOUT_SECS", "10");
        env.insert("ORION_LOGGING__LEVEL", "warn");
        env.insert("ORION_LOGGING__FORMAT", "json");
        env.insert("ORION_INGEST__MAX_PAYLOAD_SIZE", "2000000");
        env.insert("ORION_QUEUE__WORKERS", "8");
        env.insert("ORION_QUEUE__BUFFER_SIZE", "2000");
        env.insert("ORION_QUEUE__SHUTDOWN_TIMEOUT_SECS", "60");
        env.insert("ORION_METRICS__ENABLED", "true");
        env.insert("ORION_TRACING__ENABLED", "true");
        env.insert("ORION_TRACING__OTLP_ENDPOINT", "http://jaeger:4317");
        env.insert("ORION_TRACING__SERVICE_NAME", "my-orion");
        env.insert("ORION_TRACING__SAMPLE_RATE", "0.5");
        env.insert("ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS", "5");
        env.insert("ORION_ENGINE__RELOAD_TIMEOUT_SECS", "20");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__ENABLED", "true");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD", "10");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS", "60");
        env.insert("ORION_RATE_LIMIT__ENABLED", "true");
        env.insert("ORION_RATE_LIMIT__DEFAULT_RPS", "200");
        env.insert("ORION_RATE_LIMIT__DEFAULT_BURST", "100");
        env.insert("ORION_KAFKA__ENABLED", "true");
        env.insert("ORION_KAFKA__BROKERS", "broker1:9092,broker2:9092");
        env.insert("ORION_KAFKA__GROUP_ID", "my-group");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).unwrap();

        assert_eq!(config.server.host, "localhost");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.storage.path, "test.db");
        assert_eq!(config.storage.busy_timeout_ms, 10000);
        assert_eq!(config.storage.acquire_timeout_secs, 10);
        assert_eq!(config.logging.level, "warn");
        assert!(matches!(config.logging.format, LogFormat::Json));
        assert_eq!(config.ingest.max_payload_size, 2000000);
        assert_eq!(config.queue.workers, 8);
        assert_eq!(config.queue.buffer_size, 2000);
        assert_eq!(config.queue.shutdown_timeout_secs, 60);
        assert!(config.metrics.enabled);
        assert!(config.tracing.enabled);
        assert_eq!(config.tracing.otlp_endpoint, "http://jaeger:4317");
        assert_eq!(config.tracing.service_name, "my-orion");
        assert!((config.tracing.sample_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.engine.health_check_timeout_secs, 5);
        assert_eq!(config.engine.reload_timeout_secs, 20);
        assert!(config.engine.circuit_breaker.enabled);
        assert_eq!(config.engine.circuit_breaker.failure_threshold, 10);
        assert_eq!(config.engine.circuit_breaker.recovery_timeout_secs, 60);
        assert!(config.rate_limit.enabled);
        assert_eq!(config.rate_limit.default_rps, 200);
        assert_eq!(config.rate_limit.default_burst, 100);
        assert!(config.kafka.enabled);
        assert_eq!(config.kafka.brokers, vec!["broker1:9092", "broker2:9092"]);
        assert_eq!(config.kafka.group_id, "my-group");
    }

    #[test]
    fn test_env_override_format_pretty() {
        let mut env = HashMap::new();
        env.insert("ORION_LOGGING__FORMAT", "pretty");

        let mut config = AppConfig::default();
        config.logging.format = LogFormat::Json;
        apply_env_overrides_with(&mut config, make_env_reader(&env)).unwrap();

        assert!(matches!(config.logging.format, LogFormat::Pretty));
    }

    #[test]
    fn test_env_override_invalid_format_errors() {
        let mut env = HashMap::new();
        env.insert("ORION_LOGGING__FORMAT", "xml");

        let mut config = AppConfig::default();
        let result = apply_env_overrides_with(&mut config, make_env_reader(&env));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ORION_LOGGING__FORMAT")
        );
    }

    #[test]
    fn test_env_override_invalid_port_errors() {
        let mut env = HashMap::new();
        env.insert("ORION_SERVER__PORT", "not-a-number");

        let mut config = AppConfig::default();
        let result = apply_env_overrides_with(&mut config, make_env_reader(&env));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ORION_SERVER__PORT")
        );
    }

    #[test]
    fn test_env_override_invalid_bool_errors() {
        let mut env = HashMap::new();
        env.insert("ORION_METRICS__ENABLED", "yes");

        let mut config = AppConfig::default();
        let result = apply_env_overrides_with(&mut config, make_env_reader(&env));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ORION_METRICS__ENABLED")
        );
    }

    #[test]
    fn test_validate_config_zero_health_check_timeout() {
        let mut config = AppConfig::default();
        config.engine.health_check_timeout_secs = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_zero_reload_timeout() {
        let mut config = AppConfig::default();
        config.engine.reload_timeout_secs = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_zero_shutdown_timeout() {
        let mut config = AppConfig::default();
        config.queue.shutdown_timeout_secs = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_zero_busy_timeout() {
        let mut config = AppConfig::default();
        config.storage.busy_timeout_ms = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_zero_acquire_timeout() {
        let mut config = AppConfig::default();
        config.storage.acquire_timeout_secs = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config(Some("/nonexistent/path/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_toml_parsing_with_rate_limit() {
        let toml_str = r#"
[server]
port = 8080

[rate_limit]
enabled = true
default_rps = 200
default_burst = 100

[rate_limit.endpoints]
admin_rps = 50
data_rps = 500

[rate_limit.channels.orders]
rps = 100
burst = 50
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.rate_limit.enabled);
        assert_eq!(config.rate_limit.default_rps, 200);
        assert_eq!(config.rate_limit.default_burst, 100);
        assert_eq!(config.rate_limit.endpoints.admin_rps, Some(50));
        assert_eq!(config.rate_limit.endpoints.data_rps, Some(500));
        let orders = config.rate_limit.channels.get("orders").unwrap();
        assert_eq!(orders.rps, 100);
        assert_eq!(orders.burst, Some(50));
    }

    #[test]
    fn test_cors_config_default() {
        let config = CorsConfig::default();
        assert_eq!(config.allowed_origins, vec!["*"]);
    }

    #[test]
    fn test_kafka_ingest_config_default() {
        let config = KafkaIngestConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.brokers, vec!["localhost:9092"]);
        assert_eq!(config.group_id, "orion");
        assert!(config.topics.is_empty());
        assert!(!config.dlq.enabled);
        assert_eq!(config.dlq.topic, "orion-dlq");
    }

    #[test]
    fn test_tracing_config_default() {
        let config = TracingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.otlp_endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "orion");
        assert!((config.sample_rate - 1.0).abs() < f64::EPSILON);
    }
}
