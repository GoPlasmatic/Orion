use crate::config::AppConfig;
use crate::errors::OrionError;

/// Return a config error if `value` is zero. Shared helper for per-struct
/// `validate()` implementations in sibling config modules.
pub(super) fn require_nonzero(value: u64, field: &str) -> Result<(), OrionError> {
    if value == 0 {
        return Err(OrionError::Config {
            message: format!("{field} must be > 0"),
        });
    }
    Ok(())
}

/// Return a config error if `value` is empty.
pub(super) fn require_nonempty(value: &str, field: &str) -> Result<(), OrionError> {
    if value.is_empty() {
        return Err(OrionError::Config {
            message: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

/// Orchestrate validation across config sub-structs. Each `validate()`
/// method is defined next to its struct; this function only sequences them.
pub(super) fn validate_config(config: &AppConfig) -> Result<(), OrionError> {
    let is_prod = config.is_production();
    config.server.validate()?;
    config.ingest.validate()?;
    config.storage.validate()?;
    config.logging.validate()?;
    config.tracing.validate()?;
    config.admin_auth.validate(is_prod)?;
    config.cors.validate(is_prod)?;
    config.engine.validate()?;
    config.queue.validate()?;
    config.rate_limit.validate()?;
    config.kafka.validate()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdminAuthConfig, CorsConfig, TopicMapping};

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
    fn test_validate_config_empty_storage_url() {
        let mut config = AppConfig::default();
        config.storage.url = String::new();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_invalid_log_level() {
        let mut config = AppConfig::default();
        config.logging.level = "invalid".to_string();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_kafka_enabled_no_brokers() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.brokers = vec![];
        assert!(validate_config(&config).is_err());
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
    fn test_validate_config_kafka_empty_group_id() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.group_id = String::new();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_kafka_duplicate_topics() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.topics = vec![
            TopicMapping {
                topic: "dup".into(),
                channel: "ch1".into(),
            },
            TopicMapping {
                topic: "dup".into(),
                channel: "ch2".into(),
            },
        ];
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_kafka_duplicate_channels() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.topics = vec![
            TopicMapping {
                topic: "t1".into(),
                channel: "same".into(),
            },
            TopicMapping {
                topic: "t2".into(),
                channel: "same".into(),
            },
        ];
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_kafka_processing_timeout_zero() {
        let mut config = AppConfig::default();
        config.queue.processing_timeout_ms = 0;
        assert!(validate_config(&config).is_err());
    }

    // ---- Admin auth config tests ----

    #[test]
    fn test_validate_config_admin_auth_enabled_empty_key() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        config.admin_auth.api_keys = vec![];
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_admin_auth_enabled_valid() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        config.admin_auth.api_keys = vec!["my-secret-key".to_string()];
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_admin_auth_disabled_empty_key_ok() {
        let config = AppConfig::default();
        // Auth disabled with empty key should be fine
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_production_admin_auth_disabled_error() {
        let config = AppConfig {
            environment: "production".to_string(),
            admin_auth: AdminAuthConfig {
                enabled: false,
                ..AppConfig::default().admin_auth
            },
            ..AppConfig::default()
        };
        // Production + disabled admin auth should fail
        let result = validate_config(&config);
        assert!(result.is_err());
        let err = result.expect_err("test").to_string();
        assert!(err.contains("admin_auth must be enabled"));
    }

    #[test]
    fn test_validate_config_production_admin_auth_enabled_ok() {
        let config = AppConfig {
            environment: "production".to_string(),
            admin_auth: AdminAuthConfig {
                enabled: true,
                api_keys: vec!["secret-key-12345".to_string()],
                ..AppConfig::default().admin_auth
            },
            // Must also fix CORS for production
            cors: CorsConfig {
                allowed_origins: vec!["https://example.com".to_string()],
            },
            ..AppConfig::default()
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_non_production_admin_auth_disabled_ok() {
        let config = AppConfig::default();
        // Non-production + disabled admin auth should be fine (just warns)
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_admin_auth_enabled_all_empty() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        // api_keys is empty
        let result = validate_config(&config);
        assert!(result.is_err());
    }
}
