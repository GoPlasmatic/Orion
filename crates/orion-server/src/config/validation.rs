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
    config.server.validate(is_prod)?;
    config.ingest.validate()?;
    config.storage.validate()?;
    config.logging.validate()?;
    config.metrics.validate(&config.server)?;
    config.tracing.validate()?;
    config.trace_storage.validate()?;
    config.admin_auth.validate(is_prod)?;
    config.cors.validate(is_prod)?;
    config.engine.validate()?;
    config.trace_queue.validate()?;
    config.audit.validate()?;
    config.query.validate()?;
    config.write.validate()?;
    config.rate_limit.validate()?;
    config.kafka.validate()?;
    config.cluster.validate()?;
    config.cron.validate()?;
    config.vars.validate()?;
    config.secrets.validate()?;
    config.plugins.validate()?;
    // Cross-section: cluster mode is meaningless on SQLite (single-host by
    // construction) — refuse at startup rather than corrupt silently.
    // `storage.validate()` above has already established that the URL names a
    // real backend, so this is only the cluster question.
    if config.cluster.enabled
        && crate::storage::detect_backend(&config.storage.url)? == crate::storage::DbBackend::Sqlite
    {
        return Err(OrionError::Config {
            message: "cluster.enabled = true requires postgres:// or mysql:// storage \
                      (sqlite is single-host by construction)"
                .to_string(),
        });
    }
    // Cross-section (C7): a production cluster must not migrate at boot.
    //
    // Every replica would try, they race, and the guardrail used to be a log
    // line emitted *after* the race it warns about — so the safe shape of the
    // flagship feature was neither the default nor enforced. The correct shape
    // already exists everywhere it is needed: the Helm chart runs a
    // pre-install/pre-upgrade `orion-server migrate` Job, and
    // `docker-compose.ha.yml` a one-shot `migrate` service.
    //
    // Scoped to production for the same reason `admin_auth` and the CORS
    // wildcard are: a single-node install never trips it (cluster mode is off
    // by default), and a throwaway development cluster — the Helm `devStack`,
    // which deliberately has no migrate Job because its database is a release
    // resource — keeps the warning `main.rs` logs. Production cluster installs
    // are the only place a racing migration meets data worth protecting.
    if is_prod && config.cluster.enabled && config.storage.auto_migrate {
        return Err(OrionError::Config {
            message: "cluster.enabled = true with storage.auto_migrate = true in \
                      production: every replica would migrate at boot and race the \
                      others. Set storage.auto_migrate = false \
                      (ORION_STORAGE__AUTO_MIGRATE=false) and run `orion-server \
                      migrate` as a deploy step — the Helm chart ships a pre-upgrade \
                      Job and docker-compose.ha.yml a one-shot migrate service. \
                      Startup then fails fast on a pending migration instead of \
                      serving against a schema it does not understand."
                .to_string(),
        });
    }
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
        config.trace_queue.workers = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_invalid_queue_buffer() {
        let mut config = AppConfig::default();
        config.trace_queue.buffer_size = 0;
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
    fn test_validate_config_zero_shutdown_timeout() {
        let mut config = AppConfig::default();
        config.trace_queue.shutdown_timeout_secs = 0;
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
        config.trace_queue.processing_timeout_ms = 0;
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
                // >= 32 chars: production rejects weaker plaintext keys (S12).
                api_keys: vec!["secret-key-12345-secret-key-12345".to_string()],
                ..AppConfig::default().admin_auth
            },
            // Must also fix CORS for production
            cors: CorsConfig {
                allowed_origins: vec!["https://example.com".to_string()],
                ..Default::default()
            },
            ..AppConfig::default()
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_cors_wildcard_mixed_with_origins() {
        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec!["*".to_string(), "https://x.com".to_string()];
        let result = validate_config(&config);
        assert!(result.is_err());
        let err = result.expect_err("test").to_string();
        assert!(err.contains("cannot mix"));
    }

    #[test]
    fn test_validate_config_cors_explicit_origins_ok() {
        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec![
            "https://a.example.com".to_string(),
            "https://b.example.com".to_string(),
        ];
        assert!(validate_config(&config).is_ok());
    }

    /// The rule that matters most: without it, tower-http's
    /// `ensure_usable_cors_rules` asserts inside `Layer::layer` and the process
    /// **panics at boot** rather than failing config validation. The Fetch spec
    /// forbids the pairing independently, so it could never have worked.
    #[test]
    fn test_validate_config_cors_credentials_with_wildcard_origin_refused() {
        let mut config = AppConfig::default();
        config.cors.allow_credentials = true;
        // `allowed_origins` is `["*"]` by default.
        let err = validate_config(&config)
            .expect_err("credentials + wildcard must fail validation, not panic the server")
            .to_string();
        assert!(err.contains("allow_credentials"), "{err}");
    }

    #[test]
    fn test_validate_config_cors_credentials_with_explicit_origin_ok() {
        let mut config = AppConfig::default();
        config.cors.allow_credentials = true;
        config.cors.allowed_origins = vec!["https://app.example.com".to_string()];
        assert!(validate_config(&config).is_ok());
    }

    /// `AllowHeaders::list(["*"])` renders as `*` and reports itself as a
    /// wildcard, so one such entry would silently convert the explicit list
    /// back into a wildcard and re-arm the credentials assert.
    #[test]
    fn test_validate_config_cors_wildcard_in_header_list_refused() {
        for field in ["allowed", "exposed"] {
            let mut config = AppConfig::default();
            config.cors.allowed_origins = vec!["https://app.example.com".to_string()];
            if field == "allowed" {
                config.cors.additional_allowed_headers = vec!["*".to_string()];
            } else {
                config.cors.additional_exposed_headers = vec!["*".to_string()];
            }
            let err = validate_config(&config)
                .expect_err("a wildcard header entry must be refused")
                .to_string();
            assert!(err.contains("must not contain '*'"), "{field}: {err}");
        }
    }

    /// Refused, not dropped with a warning the way an unparseable *origin* is:
    /// a silently ignored header is a preflight that fails for reasons nothing
    /// explains.
    #[test]
    fn test_validate_config_cors_unparseable_header_refused() {
        let mut config = AppConfig::default();
        config.cors.allowed_origins = vec!["https://app.example.com".to_string()];
        config.cors.additional_allowed_headers = vec!["device id".to_string()];
        let err = validate_config(&config)
            .expect_err("a header name with a space cannot reach the wire")
            .to_string();
        assert!(err.contains("not a valid HTTP header name"), "{err}");
    }

    #[test]
    fn test_validate_config_cors_max_age_cap() {
        let mut config = AppConfig::default();
        config.cors.max_age_secs = Some(86_401);
        let err = validate_config(&config)
            .expect_err("a silently-clamped value must be a startup error")
            .to_string();
        assert!(err.contains("max_age_secs"), "{err}");

        config.cors.max_age_secs = Some(86_400);
        assert!(validate_config(&config).is_ok(), "the cap itself is legal");
    }

    #[test]
    fn test_validate_config_non_production_admin_auth_disabled_ok() {
        let config = AppConfig::default();
        // Non-production + disabled admin auth should be fine (just warns)
        assert!(validate_config(&config).is_ok());
    }

    // ---- C7: a production cluster must not migrate at boot ----

    /// A production cluster config with everything else in order, so only the
    /// `auto_migrate`/`cluster` pair can fail it.
    fn production_cluster_config(auto_migrate: bool) -> AppConfig {
        let mut config = AppConfig {
            environment: "production".to_string(),
            admin_auth: AdminAuthConfig {
                enabled: true,
                api_keys: vec!["secret-key-12345-secret-key-12345".to_string()],
                ..AppConfig::default().admin_auth
            },
            cors: CorsConfig {
                allowed_origins: vec!["https://example.com".to_string()],
                ..Default::default()
            },
            ..AppConfig::default()
        };
        // Cluster mode requires a networked backend and shared Redis.
        config.storage.url = "postgres://orion:orion@db:5432/orion".to_string();
        config.cluster.enabled = true;
        config.cluster.redis_url = "redis://redis:6379".to_string();
        config.storage.auto_migrate = auto_migrate;
        config
    }

    /// The defect C7 names: the warning fired *after* the replicas had already
    /// raced. It is a startup refusal now, and the message has to name both
    /// settings and the deploy step that replaces them.
    #[test]
    fn production_cluster_refuses_auto_migrate() {
        let err = validate_config(&production_cluster_config(true))
            .expect_err("racing migrations must not be a mere warning");
        let message = err.to_string();
        assert!(message.contains("storage.auto_migrate"), "{message}");
        assert!(message.contains("cluster.enabled"), "{message}");
        assert!(message.contains("orion-server migrate"), "{message}");
    }

    /// The shape the Helm chart and `docker-compose.ha.yml` already deploy.
    #[test]
    fn production_cluster_accepts_the_migrate_job_shape() {
        validate_config(&production_cluster_config(false))
            .expect("auto_migrate = false plus a migrate deploy step is the intended shape");
    }

    /// A single-node install is untouched: cluster mode is off by default and
    /// `auto_migrate` stays on, which is what makes `orion-server` a
    /// single-binary install.
    #[test]
    fn a_single_node_install_still_migrates_at_boot() {
        let config = AppConfig::default();
        assert!(config.storage.auto_migrate, "default must stay true");
        assert!(!config.cluster.enabled, "default must stay single-node");
        validate_config(&config).expect("the default install is unaffected");
    }

    /// Development clusters keep the warning: the Helm `devStack` install runs
    /// cluster mode with no migrate Job, because its database is a release
    /// resource that does not exist when a pre-install hook would run.
    #[test]
    fn a_development_cluster_still_migrates_at_boot() {
        let mut config = production_cluster_config(true);
        config.environment = "development".to_string();
        validate_config(&config)
            .expect("a throwaway development cluster is not refused, only warned");
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
