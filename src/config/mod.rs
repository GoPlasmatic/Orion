mod admin_auth;
mod engine;
mod env_overrides;
pub(crate) mod env_substitute;
mod kafka;
mod logging;
mod observability;
mod queue;
mod rate_limit;
mod server;
mod storage;
mod validation;

// Re-export all types so `use crate::config::Foo` keeps working.
pub use admin_auth::AdminAuthConfig;
pub use engine::EngineConfig;
pub use kafka::{DlqConfig, KafkaIngestConfig, TopicMapping};
pub use logging::{LogFormat, LoggingConfig};
pub use observability::{
    AsyncOnOverflow, CorsConfig, MetricsConfig, TraceStorageMode, TracingConfig,
    TracingStorageConfig,
};
pub use queue::QueueConfig;
pub use rate_limit::{EndpointRateLimits, RateLimitConfig};
pub use server::{CompressionConfig, IngestConfig, ServerConfig, TlsConfig};
pub use storage::StorageConfig;

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::errors::OrionError;

/// Top-level application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Deployment environment (e.g. "development", "production").
    /// Controls safety checks like CORS wildcard rejection.
    /// Override via `ORION_ENV`.
    #[serde(default = "default_environment")]
    pub environment: String,
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
    pub channels: ChannelLoadingConfig,
    pub admin_auth: AdminAuthConfig,
}

fn default_environment() -> String {
    "development".to_string()
}

impl AppConfig {
    /// Returns true when the environment is a production variant.
    pub fn is_production(&self) -> bool {
        self.environment.to_lowercase().starts_with("prod")
    }
}

/// Controls which channels an Orion instance loads from the database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelLoadingConfig {
    /// Glob patterns for channels to include. Empty means include all.
    pub include: Vec<String>,
    /// Glob patterns for channels to exclude. Applied after include.
    pub exclude: Vec<String>,
}

/// Load configuration from an optional TOML file path, then apply env overrides.
///
/// `${VAR}` and `${VAR:-default}` placeholders in the TOML file are
/// substituted from the process environment before TOML parsing. This
/// lets secrets stay out of the config file without forcing every value
/// to be redeclared as an `ORION_*` env var. See `env_substitute` for
/// the full grammar (including the `$$` literal-dollar escape).
pub fn load_config(path: Option<&str>) -> Result<AppConfig, OrionError> {
    let mut config = if let Some(p) = path {
        let raw =
            std::fs::read_to_string(Path::new(p)).map_err(|e| OrionError::InternalSource {
                context: format!("Failed to read config file '{}'", p),
                source: Box::new(e),
            })?;
        let content = env_substitute::substitute(&raw, p)?;
        toml::from_str::<AppConfig>(&content).map_err(|e| OrionError::InternalSource {
            context: format!("Failed to parse config file '{}'", p),
            source: Box::new(e),
        })?
    } else {
        AppConfig::default()
    };

    env_overrides::apply_env_overrides(&mut config)?;
    validation::validate_config(&config)?;

    Ok(config)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.storage.url, "sqlite:orion.db");
        assert_eq!(config.storage.max_connections, 50);
        assert_eq!(config.storage.busy_timeout_ms, 5000);
        assert_eq!(config.storage.acquire_timeout_secs, 3);
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
    fn test_toml_parsing() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 3000

[storage]
url = "sqlite:test.db"

[logging]
level = "debug"
format = "json"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.storage.url, "sqlite:test.db");
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config(Some("/nonexistent/path/config.toml"));
        assert!(result.is_err());
    }

    /// Helper for tests below: writes `content` to a unique temp file
    /// and returns its path. Path lives until test process exits.
    fn write_temp_toml(content: &str, suffix: &str) -> String {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "orion-test-config-{}-{}.toml",
            suffix,
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn test_load_config_substitutes_env_vars() {
        // SAFETY: this test sets a unique env var per invocation and only
        // reads it back from this same test — no shared state with other tests.
        let var_name = "ORION_TEST_SUBST_DB_URL";
        // SAFETY: single-threaded inside this test, no parallel reader of this var.
        unsafe {
            std::env::set_var(var_name, "postgres://test-host/db");
        }
        let toml = format!(
            r#"
[server]
port = 8080

[storage]
url = "${{{var_name}}}"
"#
        );
        let path = write_temp_toml(&toml, "subst");
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.storage.url, "postgres://test-host/db");
        unsafe {
            std::env::remove_var(var_name);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_uses_default_when_var_missing() {
        let toml = r#"
[server]
port = 8080

[storage]
url = "${ORION_TEST_NEVER_SET_VAR:-sqlite:fallback.db}"
"#;
        let path = write_temp_toml(toml, "default");
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.storage.url, "sqlite:fallback.db");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_fails_on_missing_required_var() {
        let toml = r#"
[storage]
url = "${ORION_TEST_REQUIRED_BUT_UNSET_xyz}"
"#;
        let path = write_temp_toml(toml, "missing");
        let result = load_config(Some(&path));
        let err = result.expect_err("substitution must fail when required var is unset");
        match err {
            OrionError::Config { message } => {
                assert!(message.contains("ORION_TEST_REQUIRED_BUT_UNSET_xyz"));
            }
            other => panic!("expected Config error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
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
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.rate_limit.enabled);
        assert_eq!(config.rate_limit.default_rps, 200);
        assert_eq!(config.rate_limit.default_burst, 100);
        assert_eq!(config.rate_limit.endpoints.admin_rps, Some(50));
        assert_eq!(config.rate_limit.endpoints.data_rps, Some(500));
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

    #[test]
    fn test_toml_parsing_admin_auth() {
        let toml_str = r#"
[admin_auth]
enabled = true
api_key = "my-key"
header = "X-Custom-Auth"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.admin_auth.enabled);
        assert_eq!(config.admin_auth.api_key, "my-key");
        assert_eq!(config.admin_auth.header, "X-Custom-Auth");
    }

    #[test]
    fn test_toml_parsing_admin_auth_api_keys() {
        let toml_str = r#"
[admin_auth]
enabled = true
api_keys = ["key-a", "key-b"]
header = "Authorization"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.admin_auth.enabled);
        assert_eq!(
            config.admin_auth.api_keys,
            vec!["key-a".to_string(), "key-b".to_string()]
        );
    }
}
