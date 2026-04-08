use std::path::Path;

use crate::config::AppConfig;
use crate::errors::OrionError;

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
pub(super) fn validate_config(config: &AppConfig) -> Result<(), OrionError> {
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
    if config.storage.url.is_empty() {
        return Err(OrionError::Config {
            message: "storage.url must not be empty".to_string(),
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
    // Admin auth validation
    if config.admin_auth.enabled && config.admin_auth.effective_keys().is_empty() {
        return Err(OrionError::Config {
            message: "At least one admin API key must be configured when admin auth is enabled. \
                      Set admin_auth.api_key or admin_auth.api_keys"
                .to_string(),
        });
    }
    // Admin auth must be enabled in production
    if !config.admin_auth.enabled {
        if config.is_production() {
            return Err(OrionError::Config {
                message: "admin_auth must be enabled when environment starts with 'prod'. \
                          Set admin_auth.enabled = true and configure an api_key"
                    .to_string(),
            });
        }
        tracing::warn!(
            "Admin auth is disabled. For production, enable admin_auth with a strong API key"
        );
    }
    // TLS validation
    if config.server.tls.enabled {
        if config.server.tls.cert_path.is_empty() {
            return Err(OrionError::Config {
                message: "server.tls.cert_path must not be empty when TLS is enabled".to_string(),
            });
        }
        if config.server.tls.key_path.is_empty() {
            return Err(OrionError::Config {
                message: "server.tls.key_path must not be empty when TLS is enabled".to_string(),
            });
        }
        if !Path::new(&config.server.tls.cert_path).exists() {
            return Err(OrionError::Config {
                message: format!(
                    "TLS certificate file not found: '{}'",
                    config.server.tls.cert_path
                ),
            });
        }
        if !Path::new(&config.server.tls.key_path).exists() {
            return Err(OrionError::Config {
                message: format!(
                    "TLS private key file not found: '{}'",
                    config.server.tls.key_path
                ),
            });
        }
    }
    if config.engine.max_channel_call_depth == 0 {
        return Err(OrionError::Config {
            message: "engine.max_channel_call_depth must be > 0".to_string(),
        });
    }
    require_nonzero(
        config.engine.default_channel_call_timeout_ms,
        "engine.default_channel_call_timeout_ms",
    )?;
    require_nonzero(
        config.queue.processing_timeout_ms,
        "queue.processing_timeout_ms",
    )?;
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
    // CORS: reject wildcard in production
    if config.cors.allowed_origins.len() == 1 && config.cors.allowed_origins[0] == "*" {
        if config.is_production() {
            return Err(OrionError::Config {
                message: "CORS wildcard '*' is not allowed when environment starts with 'prod'. \
                          Set explicit origins in [cors] allowed_origins"
                    .to_string(),
            });
        }
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
        // Validate broker address format (host:port)
        for (i, broker) in config.kafka.brokers.iter().enumerate() {
            let broker = broker.trim();
            if broker.is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.brokers[{}] must not be empty", i),
                });
            }
            if !broker.contains(':') {
                return Err(OrionError::Config {
                    message: format!(
                        "kafka.brokers[{}] '{}' must be in host:port format",
                        i, broker
                    ),
                });
            }
            let port_str = broker.rsplit(':').next().unwrap_or("");
            if port_str.parse::<u16>().is_err() {
                return Err(OrionError::Config {
                    message: format!("kafka.brokers[{}] '{}' has invalid port", i, broker),
                });
            }
        }
        if config.kafka.max_inflight == 0 {
            return Err(OrionError::Config {
                message: "kafka.max_inflight must be > 0".to_string(),
            });
        }
        // Topics can be empty in config when async channels provide them from DB
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
    use crate::config::TopicMapping;

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
    #[cfg(feature = "kafka")]
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
    #[cfg(feature = "kafka")]
    fn test_validate_config_kafka_empty_group_id() {
        let mut config = AppConfig::default();
        config.kafka.enabled = true;
        config.kafka.group_id = String::new();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    #[cfg(feature = "kafka")]
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
    #[cfg(feature = "kafka")]
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
        config.admin_auth.api_key = String::new();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_admin_auth_enabled_valid() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        config.admin_auth.api_key = "my-secret-key".to_string();
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
        let mut config = AppConfig::default();
        config.environment = "production".to_string();
        config.admin_auth.enabled = false;
        // Production + disabled admin auth should fail
        let result = validate_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("admin_auth must be enabled"));
    }

    #[test]
    fn test_validate_config_production_admin_auth_enabled_ok() {
        let mut config = AppConfig::default();
        config.environment = "production".to_string();
        config.admin_auth.enabled = true;
        config.admin_auth.api_key = "secret-key-12345".to_string();
        // Must also fix CORS for production
        config.cors.allowed_origins = vec!["https://example.com".to_string()];
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_non_production_admin_auth_disabled_ok() {
        let config = AppConfig::default();
        // Non-production + disabled admin auth should be fine (just warns)
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_admin_auth_enabled_via_api_keys() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        config.admin_auth.api_keys = vec!["key-a".to_string()];
        // api_key is empty but api_keys has a value — should be valid
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_admin_auth_enabled_all_empty() {
        let mut config = AppConfig::default();
        config.admin_auth.enabled = true;
        // Both api_key and api_keys are empty
        let result = validate_config(&config);
        assert!(result.is_err());
    }
}
