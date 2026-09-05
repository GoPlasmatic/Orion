use std::cell::RefCell;
use std::collections::BTreeSet;

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

/// `ORION_` + the field path uppercased and joined with `__` — the single
/// naming rule every override follows, with no exceptions since C22 retired
/// the `ORION_ENV` alias in favour of the derived `ORION_ENVIRONMENT`.
fn env_key(path: &[&str]) -> String {
    let mut key = String::from("ORION_");
    for (i, segment) in path.iter().enumerate() {
        if i > 0 {
            key.push_str("__");
        }
        key.push_str(&segment.to_ascii_uppercase());
    }
    key
}

/// Every `ORION_*` variable the overrides below consult, in one set.
///
/// Derived by *running* them against a reader that records each name and
/// reports it unset, rather than by listing the names a second time: an
/// override that reads a variable has to ask this reader for it, so the set
/// cannot drift from the macros the way a hand-maintained list would. Nothing
/// is applied — every lookup returns `NotPresent` — so the throwaway config is
/// discarded untouched.
///
/// The one rule this relies on: an override must consult its variable
/// *unconditionally*. Every `ov*!` expansion does (`if let Ok(v) =
/// env_var(&key)`), as do the hand-written enum/list/pair cases.
///
/// `config_docs_drift_test` asserts this set equals the one its scraper finds
/// in this file's source, which is what would catch a conditional lookup.
pub fn known_env_override_keys() -> BTreeSet<String> {
    let seen = RefCell::new(BTreeSet::new());
    let mut scratch = AppConfig::default();
    // Cannot fail: every lookup reports "unset", so no value is ever parsed.
    let _ = apply_env_overrides_with(&mut scratch, |key| {
        seen.borrow_mut().insert(key.to_string());
        Err(std::env::VarError::NotPresent)
    });
    seen.into_inner()
}

/// Apply ORION_* environment variable overrides.
///
/// Three passes, in this order: retired names are refused with their
/// replacement (C22), `ORION_*` names that are not overrides at all are
/// refused with the nearest valid key (C4d), and only then is anything
/// applied. `referenced_by_config_file` carries the names the config file's
/// `${VAR}` placeholders resolved, which are Orion's to read even though no
/// override names them.
pub(super) fn apply_env_overrides(
    config: &mut AppConfig,
    referenced_by_config_file: &BTreeSet<String>,
) -> Result<(), OrionError> {
    crate::config::retired_env::reject_retired_env_vars(|key| std::env::var(key))?;
    crate::config::unknown_env::reject_unknown_env_vars(
        // `vars_os` rather than `vars`: a non-UTF-8 name elsewhere in the
        // environment must not panic the server at startup.
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
        &known_env_override_keys(),
        referenced_by_config_file,
    )?;
    apply_env_overrides_with(config, |key| std::env::var(key))
}

/// Testable version that accepts a custom env reader.
///
/// Every mapping below derives its variable name from the field path via
/// [`env_key`], so an override cannot silently read the wrong variable and
/// the docs drift test can predict the full set from the config structs.
/// Only shapes the naming rule cannot express are written out by hand: enum
/// fields with their own error wording, list fields, and the `topic:channel`
/// pair grammar of `ORION_KAFKA__TOPICS`.
pub(super) fn apply_env_overrides_with<F>(
    config: &mut AppConfig,
    env_var: F,
) -> Result<(), OrionError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    /// One scalar override, `FromStr`-parsed (covers `String` too):
    /// `ov!(server.port: u16)` reads `ORION_SERVER__PORT`.
    macro_rules! ov {
        ($($path:ident).+ : $ty:ty) => {{
            let key = env_key(&[$(stringify!($path)),+]);
            if let Ok(v) = env_var(&key) {
                config.$($path).+ = parse_env::<$ty>(&key, &v)?;
            }
        }};
    }

    /// `Option<T>` override. An empty value clears the setting, which is the
    /// only way to say "no limit" from an environment that already has the
    /// variable set.
    macro_rules! ov_opt {
        ($($path:ident).+ : $ty:ty) => {{
            let key = env_key(&[$(stringify!($path)),+]);
            if let Ok(v) = env_var(&key) {
                config.$($path).+ = if v.trim().is_empty() {
                    None
                } else {
                    Some(parse_env::<$ty>(&key, &v)?)
                };
            }
        }};
    }

    /// `Option<String>` override (no clearing: an empty value sets `Some("")`).
    macro_rules! ov_opt_str {
        ($($path:ident).+) => {{
            let key = env_key(&[$(stringify!($path)),+]);
            if let Ok(v) = env_var(&key) {
                config.$($path).+ = Some(v);
            }
        }};
    }

    /// Comma-separated list override; entries are trimmed and empties dropped,
    /// so an explicitly empty value means "empty list" rather than `[""]`.
    macro_rules! ov_list {
        ($($path:ident).+) => {{
            let key = env_key(&[$(stringify!($path)),+]);
            if let Ok(v) = env_var(&key) {
                config.$($path).+ = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
        }};
    }

    /// Enum override: a case-insensitive value out of a fixed set, with the
    /// accepted values named back to the operator when it is not one of them.
    /// The key is spelled out rather than derived from the field path, because
    /// the docs drift test scrapes these literals out of the source.
    macro_rules! ov_enum {
        ($key:literal, $($path:ident).+, { $($lit:literal => $variant:expr),+ $(,)? }, $expected:literal) => {{
            if let Ok(v) = env_var($key) {
                config.$($path).+ = match v.to_lowercase().as_str() {
                    $($lit => $variant,)+
                    _ => {
                        return Err(OrionError::Config {
                            message: format!(
                                "{}: invalid value '{}', expected {}",
                                $key, v, $expected
                            ),
                        });
                    }
                };
            }
        }};
    }

    // Deployment environment. `ORION_ENVIRONMENT`, derived by the rule above
    // — `ORION_ENV` was retired in 1.0 and is refused by
    // `reject_retired_env_vars` rather than silently ignored.
    ov!(environment: String);

    // Server
    ov!(server.host: String);
    ov!(server.port: u16);
    ov!(server.shutdown_drain_secs: u64);
    ov!(server.shutdown_force_timeout_secs: u64);
    ov!(server.tls.enabled: bool);
    ov!(server.tls.cert_path: String);
    ov!(server.tls.key_path: String);
    ov!(server.compression.enabled: bool);
    // Option<bool>: an empty value restores the unset default ("enabled
    // outside production").
    ov_opt!(server.docs.enabled: bool);
    // Option<bool>: an empty value restores the unset default ("verbose
    // outside production"). An explicit true is refused in production.
    ov_opt!(server.verbose_errors: bool);
    ov!(server.max_admin_body_size: usize);
    ov_list!(server.data_mounts);

    // Storage
    ov!(storage.url: String);
    ov!(storage.busy_timeout_ms: u64);
    ov!(storage.acquire_timeout_secs: u64);
    ov!(storage.connector_encryption_key: String);
    ov!(storage.backup_dir: String);
    ov_opt!(storage.backup_retention_count: u32);
    ov!(storage.max_connections: u32);
    ov!(storage.min_connections: u32);
    ov!(storage.idle_timeout_secs: u64);
    ov!(storage.auto_migrate: bool);
    ov!(storage.connect_retry_secs: u64);

    // Cluster
    ov!(cron.enabled: bool);
    ov!(cron.poll_interval_ms: u64);
    ov!(cron.workers: usize);
    ov!(cron.claim_batch_size: i64);
    ov!(cron.claim_lease_secs: u64);
    ov!(cron.heartbeat_interval_secs: u64);
    ov!(cron.misfire_grace_secs: u64);
    ov!(cron.max_catch_up: u32);
    ov!(cron.default_timeout_ms: u64);
    ov!(cron.shutdown_timeout_secs: u64);

    ov!(cluster.enabled: bool);
    ov!(cluster.redis_url: String);
    ov!(cluster.epoch_poll_interval_ms: u64);
    ov!(cluster.instance_id: String);

    // Logging
    ov!(logging.level: String);
    ov_enum!(
        "ORION_LOGGING__FORMAT",
        logging.format,
        {
            "json" => LogFormat::Json,
            "pretty" => LogFormat::Pretty,
        },
        "'json' or 'pretty'"
    );

    // Ingest
    ov!(ingest.max_payload_size: usize);

    // Trace queue
    ov!(trace_queue.workers: usize);
    ov!(trace_queue.buffer_size: usize);
    ov!(trace_queue.shutdown_timeout_secs: u64);
    ov!(trace_queue.retention_hours: u64);
    ov!(trace_queue.cleanup_interval_secs: u64);
    ov!(trace_queue.processing_timeout_ms: u64);
    ov!(trace_queue.max_result_size_bytes: usize);
    ov!(trace_queue.max_queue_memory_bytes: usize);
    ov!(trace_queue.dlq_retry_enabled: bool);
    ov!(trace_queue.dlq_max_retries: i64);
    ov!(trace_queue.dlq_poll_interval_secs: u64);
    ov!(trace_queue.dlq_batch_size: i64);
    ov!(trace_queue.dlq_lease_secs: u64);

    // Audit-log retention
    ov!(audit.retention_days: u64);
    ov!(audit.cleanup_interval_secs: u64);
    ov!(audit.max_pending: usize);
    ov!(audit.drain_timeout_secs: u64);

    // Plugins
    ov!(plugins.enabled: bool);
    ov!(plugins.cache_dir: String);
    ov!(plugins.max_component_bytes: usize);
    ov!(plugins.max_memory_bytes: usize);
    ov!(plugins.max_request_bytes: usize);
    ov!(plugins.max_response_bytes: usize);
    ov!(plugins.max_timeout_ms: u64);
    ov!(plugins.max_concurrency_per_function: u32);
    ov!(plugins.max_live_instances: u32);
    ov!(plugins.fuel_backstop: u64);
    ov_list!(plugins.trust.public_keys);

    // Query dialect
    ov!(query.default_limit: u64);
    ov!(query.max_limit: u64);
    ov!(query.max_skip: u64);

    // Write dialect
    ov!(write.max_rows: u64);
    ov!(write.allow_unfiltered: bool);

    // Metrics
    ov!(metrics.enabled: bool);
    ov_opt_str!(metrics.bind_addr);

    // Tracing
    ov!(tracing.enabled: bool);
    ov!(tracing.otlp_endpoint: String);
    ov!(tracing.service_name: String);
    ov!(tracing.sample_rate: f64);
    ov!(tracing.debug_profile_enabled: bool);
    ov!(trace_storage.sample_rate: f64);
    ov!(trace_storage.errors_only: bool);
    ov!(trace_storage.max_pending: usize);
    ov!(trace_storage.overflow_block_timeout_ms: u64);
    ov!(trace_storage.async_workers: usize);
    ov!(trace_storage.batch_size: usize);
    ov!(trace_storage.batch_flush_interval_ms: u64);
    ov!(trace_storage.batch_workers: usize);
    ov_enum!(
        "ORION_TRACE_STORAGE__MODE",
        trace_storage.mode,
        {
            "sync" => crate::config::TraceStorageMode::Sync,
            "async" => crate::config::TraceStorageMode::Async,
            "batch" => crate::config::TraceStorageMode::Batch,
            "off" => crate::config::TraceStorageMode::Off,
        },
        "'sync', 'async', 'batch', or 'off'"
    );
    ov_enum!(
        "ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW",
        trace_storage.async_on_overflow,
        {
            "drop" => crate::config::AsyncOnOverflow::Drop,
            "block" => crate::config::AsyncOnOverflow::Block,
        },
        "'drop' or 'block'"
    );

    // Engine
    ov!(engine.health_check_timeout_secs: u64);
    ov!(engine.max_channel_call_depth: u32);
    ov!(engine.default_channel_call_timeout_ms: u64);
    ov!(engine.max_loop_iterations: i64);
    ov!(engine.global_http_timeout_secs: u64);
    ov!(engine.max_pool_cache_entries: usize);
    ov!(engine.max_memory_cache_entries: usize);
    ov!(engine.rollout_sticky_header: String);
    ov!(engine.cache_cleanup_interval_secs: u64);
    ov!(engine.fail_on_connector_load_error: bool);
    ov!(engine.circuit_breaker.enabled: bool);
    ov!(engine.circuit_breaker.failure_threshold: u32);
    ov!(engine.circuit_breaker.recovery_timeout_secs: u64);
    ov!(engine.circuit_breaker.max_breakers: usize);

    // Rate limiting
    ov!(rate_limit.enabled: bool);
    ov!(rate_limit.default_rps: u32);
    ov!(rate_limit.default_burst: u32);
    ov_opt!(rate_limit.endpoints.admin_rps: u32);
    ov_opt!(rate_limit.endpoints.data_rps: u32);
    ov_list!(rate_limit.trusted_proxies);

    // Kafka
    ov!(kafka.enabled: bool);
    ov!(kafka.group_id: String);
    ov!(kafka.processing_timeout_ms: u64);
    ov!(kafka.lag_poll_interval_secs: u64);
    ov!(kafka.session_timeout_ms: u64);
    ov!(kafka.dlq.enabled: bool);
    ov!(kafka.dlq.topic: String);
    ov_opt_str!(kafka.auth.security_protocol);
    ov_opt_str!(kafka.auth.sasl_mechanism);
    ov_opt_str!(kafka.auth.sasl_username);
    ov_opt_str!(kafka.auth.sasl_password);
    ov_opt_str!(kafka.auth.ssl_ca_location);
    // Brokers keep their historical shape: trimmed, empties preserved.
    if let Ok(v) = env_var("ORION_KAFKA__BROKERS") {
        config.kafka.brokers = v.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Ok(v) = env_var("ORION_KAFKA__TOPICS") {
        let mut topics = Vec::new();
        for entry in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match entry.split_once(':') {
                Some((topic, channel))
                    if !topic.trim().is_empty() && !channel.trim().is_empty() =>
                {
                    topics.push(crate::config::TopicMapping {
                        topic: topic.trim().to_string(),
                        channel: channel.trim().to_string(),
                    });
                }
                _ => {
                    return Err(OrionError::Config {
                        message: format!(
                            "ORION_KAFKA__TOPICS entry '{entry}' is not 'topic:channel'"
                        ),
                    });
                }
            }
        }
        config.kafka.topics = topics;
    }

    // CORS
    ov_list!(cors.allowed_origins);
    ov_list!(cors.additional_allowed_headers);
    ov_list!(cors.additional_exposed_headers);
    ov!(cors.allow_credentials: bool);
    ov_opt!(cors.max_age_secs: u64);

    // Channel loading filters
    ov_list!(channel_filter.include);
    ov_list!(channel_filter.exclude);

    // JWT verification (JWKS egress policy)
    ov!(jwt.allow_private_jwks_urls: bool);

    // Inbound OAuth2 sign-in (token-endpoint egress policy)
    ov!(oauth2_login.allow_private_token_urls: bool);

    // Admin auth
    ov!(admin_auth.enabled: bool);
    ov!(admin_auth.header: String);
    ov_list!(admin_auth.api_keys);
    ov_list!(admin_auth.read_only_api_keys);

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

    /// C4d: the allowlist the unknown-variable guard works from is derived by
    /// running the overrides, so it must contain one entry per override —
    /// including the hand-written enum / list / pair shapes, which no `ov*!`
    /// macro produces.
    #[test]
    fn known_keys_cover_every_override_shape() {
        let keys = known_env_override_keys();
        for expected in [
            // `ov!`, at three nesting depths.
            "ORION_ENVIRONMENT",
            "ORION_SERVER__PORT",
            "ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD",
            // `ov_opt!` / `ov_opt_str!` / `ov_list!`.
            "ORION_SERVER__DOCS__ENABLED",
            "ORION_KAFKA__AUTH__SASL_PASSWORD",
            "ORION_CORS__ALLOWED_ORIGINS",
            // Hand-written: enum, enum, list, pair grammar.
            "ORION_LOGGING__FORMAT",
            "ORION_TRACE_STORAGE__MODE",
            "ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW",
            "ORION_KAFKA__BROKERS",
            "ORION_KAFKA__TOPICS",
        ] {
            assert!(keys.contains(expected), "{expected} is not in the key set");
        }
        assert!(
            keys.len() > 90,
            "only {} keys derived — the recording reader is not seeing the \
             overrides, which would make the guard reject real settings",
            keys.len()
        );
        // `env_key` builds its name from the bare prefix upwards; only the
        // finished names may reach the set.
        assert!(!keys.contains("ORION_"), "the bare prefix is not a key");
    }

    /// Probing for the key set must leave the config exactly as it found it.
    ///
    /// The recording reader answers `NotPresent` to everything, so nothing can
    /// be applied — but that is a property of the reader, and the assertion has
    /// to be made against the config the derivation actually walks. Driving
    /// `apply_env_overrides_with` here rather than calling
    /// `known_env_override_keys` (which mutates a scratch config it then drops)
    /// is what makes this test able to fail: swap the reader for one that
    /// returns values and it does.
    #[test]
    fn deriving_the_key_set_applies_nothing() {
        let seen = RefCell::new(BTreeSet::new());
        let mut scratch = AppConfig::default();
        apply_env_overrides_with(&mut scratch, |key| {
            seen.borrow_mut().insert(key.to_string());
            Err(std::env::VarError::NotPresent)
        })
        .expect("a reader that reports everything unset cannot fail");

        let untouched = AppConfig::default();
        assert_eq!(
            toml::Value::try_from(&scratch).expect("config serializes"),
            toml::Value::try_from(&untouched).expect("config serializes"),
            "the key-set derivation modified the config it walked"
        );
        // …and it did walk it: the same reader is what produces the allowlist.
        assert_eq!(seen.into_inner(), known_env_override_keys());
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
        .expect("test");
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
        env.insert("ORION_TRACE_QUEUE__WORKERS", "8");
        env.insert("ORION_TRACE_QUEUE__BUFFER_SIZE", "2000");
        env.insert("ORION_TRACE_QUEUE__SHUTDOWN_TIMEOUT_SECS", "60");
        env.insert("ORION_AUDIT__RETENTION_DAYS", "30");
        env.insert("ORION_TRACE_QUEUE__DLQ_RETRY_ENABLED", "false");
        env.insert("ORION_TRACE_QUEUE__DLQ_MAX_RETRIES", "9");
        env.insert("ORION_TRACE_QUEUE__DLQ_POLL_INTERVAL_SECS", "45");
        env.insert("ORION_TRACE_QUEUE__DLQ_BATCH_SIZE", "40");
        env.insert("ORION_TRACE_QUEUE__DLQ_LEASE_SECS", "90");
        env.insert("ORION_STORAGE__MAX_CONNECTIONS", "77");
        env.insert("ORION_STORAGE__MIN_CONNECTIONS", "7");
        env.insert("ORION_STORAGE__IDLE_TIMEOUT_SECS", "600");
        env.insert("ORION_STORAGE__BACKUP_RETENTION_COUNT", "12");
        env.insert("ORION_STORAGE__AUTO_MIGRATE", "false");
        env.insert("ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS", "45");
        env.insert("ORION_KAFKA__SESSION_TIMEOUT_MS", "30000");
        env.insert("ORION_CLUSTER__ENABLED", "true");
        env.insert("ORION_CLUSTER__REDIS_URL", "redis://cluster-redis:6379");
        env.insert("ORION_CLUSTER__EPOCH_POLL_INTERVAL_MS", "500");
        env.insert("ORION_CLUSTER__INSTANCE_ID", "node-a");
        env.insert("ORION_METRICS__ENABLED", "true");
        env.insert("ORION_TRACING__ENABLED", "true");
        env.insert("ORION_TRACING__OTLP_ENDPOINT", "http://jaeger:4317");
        env.insert("ORION_TRACING__SERVICE_NAME", "my-orion");
        env.insert("ORION_TRACING__SAMPLE_RATE", "0.5");
        env.insert("ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS", "5");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__ENABLED", "true");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD", "10");
        env.insert("ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS", "60");
        env.insert("ORION_RATE_LIMIT__ENABLED", "true");
        env.insert("ORION_RATE_LIMIT__DEFAULT_RPS", "200");
        env.insert("ORION_RATE_LIMIT__DEFAULT_BURST", "100");
        env.insert("ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS", "5");
        env.insert("ORION_RATE_LIMIT__ENDPOINTS__DATA_RPS", "500");
        env.insert("ORION_SERVER__COMPRESSION__ENABLED", "true");
        env.insert("ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS", "15");
        env.insert("ORION_KAFKA__ENABLED", "true");
        env.insert("ORION_KAFKA__BROKERS", "broker1:9092,broker2:9092");
        env.insert("ORION_KAFKA__GROUP_ID", "my-group");
        env.insert("ORION_KAFKA__TOPICS", "orders:order-ch, events:event-ch");
        env.insert("ORION_KAFKA__DLQ__ENABLED", "true");
        env.insert("ORION_KAFKA__DLQ__TOPIC", "my-dlq");
        env.insert(
            "ORION_CORS__ALLOWED_ORIGINS",
            "https://a.example, https://b.example",
        );
        env.insert(
            "ORION_CORS__ADDITIONAL_ALLOWED_HEADERS",
            "deviceid, x-partner",
        );
        env.insert("ORION_CORS__ADDITIONAL_EXPOSED_HEADERS", "set-cookie");
        env.insert("ORION_CORS__ALLOW_CREDENTIALS", "true");
        env.insert("ORION_CORS__MAX_AGE_SECS", "600");
        env.insert("ORION_CHANNEL_FILTER__INCLUDE", "orders-*, payments-*");
        env.insert("ORION_CHANNEL_FILTER__EXCLUDE", "internal-*");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert_eq!(config.server.host, "localhost");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.storage.url, "sqlite:test.db");
        assert_eq!(config.storage.busy_timeout_ms, 10000);
        assert_eq!(config.storage.acquire_timeout_secs, 10);
        assert_eq!(config.logging.level, "warn");
        assert!(matches!(config.logging.format, LogFormat::Json));
        assert_eq!(config.ingest.max_payload_size, 2000000);
        assert_eq!(config.trace_queue.workers, 8);
        assert_eq!(config.trace_queue.buffer_size, 2000);
        assert_eq!(config.trace_queue.shutdown_timeout_secs, 60);
        assert_eq!(config.audit.retention_days, 30);
        assert!(!config.trace_queue.dlq_retry_enabled);
        assert_eq!(config.trace_queue.dlq_max_retries, 9);
        assert_eq!(config.trace_queue.dlq_poll_interval_secs, 45);
        assert_eq!(config.trace_queue.dlq_batch_size, 40);
        assert_eq!(config.trace_queue.dlq_lease_secs, 90);
        assert_eq!(config.storage.max_connections, 77);
        assert_eq!(config.storage.min_connections, 7);
        assert_eq!(config.storage.idle_timeout_secs, 600);
        assert_eq!(config.storage.backup_retention_count, Some(12));
        assert!(!config.storage.auto_migrate);
        assert_eq!(config.server.shutdown_force_timeout_secs, 45);
        assert_eq!(config.kafka.session_timeout_ms, 30000);
        assert!(config.cluster.enabled);
        assert_eq!(config.cluster.redis_url, "redis://cluster-redis:6379");
        assert_eq!(config.cluster.epoch_poll_interval_ms, 500);
        assert_eq!(config.cluster.instance_id, "node-a");
        assert!(config.metrics.enabled);
        assert!(config.tracing.enabled);
        assert_eq!(config.tracing.otlp_endpoint, "http://jaeger:4317");
        assert_eq!(config.tracing.service_name, "my-orion");
        assert!((config.tracing.sample_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.engine.health_check_timeout_secs, 5);
        assert!(config.engine.circuit_breaker.enabled);
        assert_eq!(config.engine.circuit_breaker.failure_threshold, 10);
        assert_eq!(config.engine.circuit_breaker.recovery_timeout_secs, 60);
        assert!(config.rate_limit.enabled);
        assert_eq!(config.rate_limit.default_rps, 200);
        assert_eq!(config.rate_limit.default_burst, 100);
        assert_eq!(config.rate_limit.endpoints.admin_rps, Some(5));
        assert_eq!(config.rate_limit.endpoints.data_rps, Some(500));
        assert!(config.server.compression.enabled);
        assert_eq!(config.engine.cache_cleanup_interval_secs, 15);
        assert!(config.kafka.enabled);
        assert_eq!(config.kafka.brokers, vec!["broker1:9092", "broker2:9092"]);
        assert_eq!(config.kafka.group_id, "my-group");
        assert_eq!(config.kafka.topics.len(), 2);
        assert_eq!(config.kafka.topics[0].topic, "orders");
        assert_eq!(config.kafka.topics[0].channel, "order-ch");
        assert_eq!(config.kafka.topics[1].topic, "events");
        assert_eq!(config.kafka.topics[1].channel, "event-ch");
        assert!(config.kafka.dlq.enabled);
        assert_eq!(config.kafka.dlq.topic, "my-dlq");
        assert_eq!(
            config.cors.allowed_origins,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        assert_eq!(
            config.cors.additional_allowed_headers,
            vec!["deviceid".to_string(), "x-partner".to_string()]
        );
        assert_eq!(
            config.cors.additional_exposed_headers,
            vec!["set-cookie".to_string()]
        );
        assert!(config.cors.allow_credentials);
        assert_eq!(config.cors.max_age_secs, Some(600));
        assert_eq!(
            config.channel_filter.include,
            vec!["orders-*".to_string(), "payments-*".to_string()]
        );
        assert_eq!(
            config.channel_filter.exclude,
            vec!["internal-*".to_string()]
        );
    }

    /// F37: an `Option<u32>` endpoint limit is cleared, not left at its
    /// previous value, when the variable is present but empty — the only way
    /// to express "no separate limit" from an environment that already sets it.
    #[test]
    fn test_env_override_empty_endpoint_limit_clears_it() {
        let mut env = HashMap::new();
        env.insert("ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS", "");

        let mut config = AppConfig::default();
        config.rate_limit.endpoints.admin_rps = Some(10);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert_eq!(config.rate_limit.endpoints.admin_rps, None);
    }

    /// S17: the docs gate is an `Option<bool>` — settable both ways, and an
    /// empty value restores "unset" (enabled outside production).
    #[test]
    fn test_env_override_docs_enabled() {
        let mut env = HashMap::new();
        env.insert("ORION_SERVER__DOCS__ENABLED", "true");
        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");
        assert_eq!(config.server.docs.enabled, Some(true));

        let mut env = HashMap::new();
        env.insert("ORION_SERVER__DOCS__ENABLED", "");
        let mut config = AppConfig::default();
        config.server.docs.enabled = Some(false);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");
        assert_eq!(config.server.docs.enabled, None);
    }

    #[test]
    fn test_env_override_invalid_endpoint_limit_errors() {
        let mut env = HashMap::new();
        env.insert("ORION_RATE_LIMIT__ENDPOINTS__DATA_RPS", "lots");

        let mut config = AppConfig::default();
        let err = apply_env_overrides_with(&mut config, make_env_reader(&env))
            .expect_err("a non-numeric endpoint limit must be rejected");
        assert!(
            err.to_string()
                .contains("ORION_RATE_LIMIT__ENDPOINTS__DATA_RPS")
        );
    }

    #[test]
    fn test_env_override_kafka_topics_rejects_malformed_entry() {
        let mut env = HashMap::new();
        env.insert("ORION_KAFKA__TOPICS", "orders:order-ch,missing-channel");
        let mut config = AppConfig::default();
        let err = apply_env_overrides_with(&mut config, make_env_reader(&env))
            .expect_err("malformed mapping must be rejected");
        assert!(err.to_string().contains("missing-channel"));
    }

    #[test]
    fn test_env_override_format_pretty() {
        let mut env = HashMap::new();
        env.insert("ORION_LOGGING__FORMAT", "pretty");

        let mut config = AppConfig::default();
        config.logging.format = LogFormat::Json;
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

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
                .expect_err("test")
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
                .expect_err("test")
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
                .expect_err("test")
                .to_string()
                .contains("ORION_METRICS__ENABLED")
        );
    }

    #[test]
    fn test_env_override_trusted_proxies() {
        let mut env = HashMap::new();
        env.insert(
            "ORION_RATE_LIMIT__TRUSTED_PROXIES",
            "10.0.0.0/8, 192.168.1.1",
        );

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert_eq!(
            config.rate_limit.trusted_proxies,
            vec!["10.0.0.0/8", "192.168.1.1"]
        );
    }

    #[test]
    fn test_env_override_kafka_auth() {
        let mut env = HashMap::new();
        env.insert("ORION_KAFKA__AUTH__SECURITY_PROTOCOL", "sasl_ssl");
        env.insert("ORION_KAFKA__AUTH__SASL_MECHANISM", "SCRAM-SHA-256");
        env.insert("ORION_KAFKA__AUTH__SASL_USERNAME", "svc-orion");
        env.insert("ORION_KAFKA__AUTH__SASL_PASSWORD", "s3cret");
        env.insert("ORION_KAFKA__AUTH__SSL_CA_LOCATION", "/etc/kafka/ca.pem");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert_eq!(
            config.kafka.auth.security_protocol.as_deref(),
            Some("sasl_ssl")
        );
        assert_eq!(
            config.kafka.auth.sasl_mechanism.as_deref(),
            Some("SCRAM-SHA-256")
        );
        assert_eq!(
            config.kafka.auth.sasl_username.as_deref(),
            Some("svc-orion")
        );
        assert_eq!(config.kafka.auth.sasl_password.as_deref(), Some("s3cret"));
        assert_eq!(
            config.kafka.auth.ssl_ca_location.as_deref(),
            Some("/etc/kafka/ca.pem")
        );
    }

    #[test]
    fn test_env_override_admin_auth() {
        let mut env = HashMap::new();
        env.insert("ORION_ADMIN_AUTH__ENABLED", "true");
        env.insert("ORION_ADMIN_AUTH__HEADER", "X-API-Key");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert!(config.admin_auth.enabled);
        assert_eq!(config.admin_auth.header, "X-API-Key");
    }

    #[test]
    fn test_env_override_admin_auth_api_keys() {
        let mut env = HashMap::new();
        env.insert("ORION_ADMIN_AUTH__API_KEYS", "key-1, key-2, key-3");

        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test");

        assert_eq!(config.admin_auth.api_keys, vec!["key-1", "key-2", "key-3"]);
    }
}
