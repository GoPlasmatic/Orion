use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::errors::OrionError;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ingest: IngestConfig,
    pub engine: EngineConfig,
    pub queue: QueueConfig,
    pub logging: LoggingConfig,
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub workers: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            workers: num_cpus::get(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: "orion.db".to_string(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub max_concurrent_workflows: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_workflows: 100,
        }
    }
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
#[derive(Default)]
pub struct MetricsConfig {
    pub enabled: bool,
}

/// Load configuration from an optional TOML file path, then apply env overrides.
pub fn load_config(path: Option<&str>) -> Result<AppConfig, OrionError> {
    let mut config = if let Some(p) = path {
        let content = std::fs::read_to_string(Path::new(p)).map_err(|e| {
            OrionError::Internal(format!("Failed to read config file '{}': {}", p, e))
        })?;
        toml::from_str::<AppConfig>(&content).map_err(|e| {
            OrionError::Internal(format!("Failed to parse config file '{}': {}", p, e))
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
    if let Ok(v) = std::env::var("ORION_SERVER__HOST") {
        config.server.host = v;
    }
    if let Ok(v) = std::env::var("ORION_SERVER__PORT")
        && let Ok(port) = v.parse::<u16>()
    {
        config.server.port = port;
    }
    if let Ok(v) = std::env::var("ORION_SERVER__WORKERS")
        && let Ok(workers) = v.parse::<usize>()
    {
        config.server.workers = workers;
    }
    if let Ok(v) = std::env::var("ORION_STORAGE__PATH") {
        config.storage.path = v;
    }
    if let Ok(v) = std::env::var("ORION_LOGGING__LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = std::env::var("ORION_LOGGING__FORMAT") {
        match v.to_lowercase().as_str() {
            "json" => config.logging.format = LogFormat::Json,
            "pretty" => config.logging.format = LogFormat::Pretty,
            _ => {}
        }
    }
    if let Ok(v) = std::env::var("ORION_INGEST__MAX_PAYLOAD_SIZE")
        && let Ok(size) = v.parse::<usize>()
    {
        config.ingest.max_payload_size = size;
    }
    if let Ok(v) = std::env::var("ORION_INGEST__BATCH_SIZE")
        && let Ok(size) = v.parse::<usize>()
    {
        config.ingest.batch_size = size;
    }
    if let Ok(v) = std::env::var("ORION_ENGINE__MAX_CONCURRENT_WORKFLOWS")
        && let Ok(max) = v.parse::<usize>()
    {
        config.engine.max_concurrent_workflows = max;
    }
    if let Ok(v) = std::env::var("ORION_QUEUE__WORKERS")
        && let Ok(w) = v.parse::<usize>()
    {
        config.queue.workers = w;
    }
    if let Ok(v) = std::env::var("ORION_QUEUE__BUFFER_SIZE")
        && let Ok(s) = v.parse::<usize>()
    {
        config.queue.buffer_size = s;
    }
    if let Ok(v) = std::env::var("ORION_METRICS__ENABLED")
        && let Ok(enabled) = v.parse::<bool>()
    {
        config.metrics.enabled = enabled;
    }
}

/// Validate configuration values.
fn validate_config(config: &AppConfig) -> Result<(), OrionError> {
    if config.server.port == 0 {
        return Err(OrionError::BadRequest(
            "server.port must be > 0".to_string(),
        ));
    }
    if config.server.workers == 0 {
        return Err(OrionError::BadRequest(
            "server.workers must be > 0".to_string(),
        ));
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
        assert!(config.server.workers > 0);
        assert_eq!(config.storage.path, "orion.db");
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
    fn test_validate_config_invalid_workers() {
        let mut config = AppConfig::default();
        config.server.workers = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_env_override() {
        // SAFETY: Test is single-threaded and we restore the var immediately after.
        unsafe {
            std::env::set_var("ORION_SERVER__PORT", "9090");
        }
        let mut config = AppConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(config.server.port, 9090);
        unsafe {
            std::env::remove_var("ORION_SERVER__PORT");
        }
    }

    #[test]
    fn test_toml_parsing() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 3000
workers = 4

[storage]
path = "test.db"

[logging]
level = "debug"
format = "json"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.server.workers, 4);
        assert_eq!(config.storage.path, "test.db");
        assert_eq!(config.logging.level, "debug");
    }
}
