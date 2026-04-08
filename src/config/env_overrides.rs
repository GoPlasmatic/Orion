use crate::config::{AppConfig, LogFormat};
use crate::errors::OrionError;

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
pub(super) fn apply_env_overrides(config: &mut AppConfig) -> Result<(), OrionError> {
    apply_env_overrides_with(config, |key| std::env::var(key))
}

/// Testable version that accepts a custom env reader.
pub(super) fn apply_env_overrides_with<F>(
    config: &mut AppConfig,
    env_var: F,
) -> Result<(), OrionError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    /// Apply a string env override.
    macro_rules! env_str {
        ($env_var:expr, $key:expr, $field:expr) => {
            if let Ok(v) = $env_var($key) {
                $field = v;
            }
        };
    }

    /// Apply a parsed env override.
    macro_rules! env_parsed {
        ($env_var:expr, $key:expr, $field:expr, $ty:ty) => {
            if let Ok(v) = $env_var($key) {
                $field = parse_env::<$ty>($key, &v)?;
            }
        };
    }

    // Environment
    env_str!(env_var, "ORION_ENV", config.environment);

    // Server
    env_str!(env_var, "ORION_SERVER__HOST", config.server.host);
    env_parsed!(env_var, "ORION_SERVER__PORT", config.server.port, u16);
    env_parsed!(
        env_var,
        "ORION_SERVER__SHUTDOWN_DRAIN_SECS",
        config.server.shutdown_drain_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_SERVER__TLS__ENABLED",
        config.server.tls.enabled,
        bool
    );
    env_str!(
        env_var,
        "ORION_SERVER__TLS__CERT_PATH",
        config.server.tls.cert_path
    );
    env_str!(
        env_var,
        "ORION_SERVER__TLS__KEY_PATH",
        config.server.tls.key_path
    );

    // Storage
    env_str!(env_var, "ORION_STORAGE__URL", config.storage.url);
    env_parsed!(
        env_var,
        "ORION_STORAGE__BUSY_TIMEOUT_MS",
        config.storage.busy_timeout_ms,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_STORAGE__ACQUIRE_TIMEOUT_SECS",
        config.storage.acquire_timeout_secs,
        u64
    );
    env_str!(
        env_var,
        "ORION_STORAGE__BACKUP_DIR",
        config.storage.backup_dir
    );

    // Logging
    env_str!(env_var, "ORION_LOGGING__LEVEL", config.logging.level);
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

    // Ingest
    env_parsed!(
        env_var,
        "ORION_INGEST__MAX_PAYLOAD_SIZE",
        config.ingest.max_payload_size,
        usize
    );

    // Queue
    env_parsed!(env_var, "ORION_QUEUE__WORKERS", config.queue.workers, usize);
    env_parsed!(
        env_var,
        "ORION_QUEUE__BUFFER_SIZE",
        config.queue.buffer_size,
        usize
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__SHUTDOWN_TIMEOUT_SECS",
        config.queue.shutdown_timeout_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__TRACE_RETENTION_HOURS",
        config.queue.trace_retention_hours,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__TRACE_CLEANUP_INTERVAL_SECS",
        config.queue.trace_cleanup_interval_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__PROCESSING_TIMEOUT_MS",
        config.queue.processing_timeout_ms,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__MAX_RESULT_SIZE_BYTES",
        config.queue.max_result_size_bytes,
        usize
    );
    env_parsed!(
        env_var,
        "ORION_QUEUE__MAX_QUEUE_MEMORY_BYTES",
        config.queue.max_queue_memory_bytes,
        usize
    );

    // Metrics
    env_parsed!(
        env_var,
        "ORION_METRICS__ENABLED",
        config.metrics.enabled,
        bool
    );

    // Tracing
    env_parsed!(
        env_var,
        "ORION_TRACING__ENABLED",
        config.tracing.enabled,
        bool
    );
    env_str!(
        env_var,
        "ORION_TRACING__OTLP_ENDPOINT",
        config.tracing.otlp_endpoint
    );
    env_str!(
        env_var,
        "ORION_TRACING__SERVICE_NAME",
        config.tracing.service_name
    );
    env_parsed!(
        env_var,
        "ORION_TRACING__SAMPLE_RATE",
        config.tracing.sample_rate,
        f64
    );

    // Engine
    env_parsed!(
        env_var,
        "ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS",
        config.engine.health_check_timeout_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__RELOAD_TIMEOUT_SECS",
        config.engine.reload_timeout_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__MAX_CHANNEL_CALL_DEPTH",
        config.engine.max_channel_call_depth,
        u32
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__DEFAULT_CHANNEL_CALL_TIMEOUT_MS",
        config.engine.default_channel_call_timeout_ms,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__GLOBAL_HTTP_TIMEOUT_SECS",
        config.engine.global_http_timeout_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__MAX_POOL_CACHE_ENTRIES",
        config.engine.max_pool_cache_entries,
        usize
    );

    // Circuit breaker
    env_parsed!(
        env_var,
        "ORION_ENGINE__CIRCUIT_BREAKER__ENABLED",
        config.engine.circuit_breaker.enabled,
        bool
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD",
        config.engine.circuit_breaker.failure_threshold,
        u32
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS",
        config.engine.circuit_breaker.recovery_timeout_secs,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_ENGINE__CIRCUIT_BREAKER__MAX_BREAKERS",
        config.engine.circuit_breaker.max_breakers,
        usize
    );

    // Rate limit
    env_parsed!(
        env_var,
        "ORION_RATE_LIMIT__ENABLED",
        config.rate_limit.enabled,
        bool
    );
    env_parsed!(
        env_var,
        "ORION_RATE_LIMIT__DEFAULT_RPS",
        config.rate_limit.default_rps,
        u32
    );
    env_parsed!(
        env_var,
        "ORION_RATE_LIMIT__DEFAULT_BURST",
        config.rate_limit.default_burst,
        u32
    );

    // Kafka
    env_parsed!(env_var, "ORION_KAFKA__ENABLED", config.kafka.enabled, bool);
    if let Ok(v) = env_var("ORION_KAFKA__BROKERS") {
        config.kafka.brokers = v.split(',').map(|s| s.trim().to_string()).collect();
    }
    env_str!(env_var, "ORION_KAFKA__GROUP_ID", config.kafka.group_id);
    env_parsed!(
        env_var,
        "ORION_KAFKA__PROCESSING_TIMEOUT_MS",
        config.kafka.processing_timeout_ms,
        u64
    );
    env_parsed!(
        env_var,
        "ORION_KAFKA__MAX_INFLIGHT",
        config.kafka.max_inflight,
        usize
    );
    env_parsed!(
        env_var,
        "ORION_KAFKA__LAG_POLL_INTERVAL_SECS",
        config.kafka.lag_poll_interval_secs,
        u64
    );

    // Admin auth
    env_parsed!(
        env_var,
        "ORION_ADMIN_AUTH__ENABLED",
        config.admin_auth.enabled,
        bool
    );
    env_str!(
        env_var,
        "ORION_ADMIN_AUTH__API_KEY",
        config.admin_auth.api_key
    );
    env_str!(
        env_var,
        "ORION_ADMIN_AUTH__HEADER",
        config.admin_auth.header
    );
    if let Ok(v) = env_var("ORION_ADMIN_AUTH__API_KEYS") {
        config.admin_auth.api_keys = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
    fn test_env_override() {
        use std::collections::HashMap;

        let mut env = HashMap::new();
        env.insert("ORION_SERVER__PORT", "9090");
        env.insert("ORION_STORAGE__URL", "postgres://localhost/orion");
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
        assert_eq!(config.storage.url, "postgres://localhost/orion");
        assert_eq!(config.logging.level, "debug");
        assert!(config.metrics.enabled);
    }

    #[test]
    fn test_env_override_all_fields() {
        let mut env = HashMap::new();
        env.insert("ORION_SERVER__HOST", "localhost");
        env.insert("ORION_SERVER__PORT", "3000");
        env.insert("ORION_STORAGE__URL", "sqlite:test.db");
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
        assert_eq!(config.storage.url, "sqlite:test.db");
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
    fn test_env_override_admin_auth() {
        let mut env = HashMap::new();
        env.insert("ORION_ADMIN_AUTH__ENABLED", "true");
        env.insert("ORION_ADMIN_AUTH__API_KEY", "secret-123");
        env.insert("ORION_ADMIN_AUTH__HEADER", "X-API-Key");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).unwrap();

        assert!(config.admin_auth.enabled);
        assert_eq!(config.admin_auth.api_key, "secret-123");
        assert_eq!(config.admin_auth.header, "X-API-Key");
    }

    #[test]
    fn test_env_override_admin_auth_api_keys() {
        let mut env = HashMap::new();
        env.insert("ORION_ADMIN_AUTH__API_KEYS", "key-1, key-2, key-3");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).unwrap();

        assert_eq!(config.admin_auth.api_keys, vec!["key-1", "key-2", "key-3"]);
    }
}
