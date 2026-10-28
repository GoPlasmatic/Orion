use std::collections::BTreeSet;
use std::sync::LazyLock;

use serde_json::Value;

use crate::config::AppConfig;
use crate::errors::OrionError;

#[derive(Clone, Copy)]
enum Shape {
    String,
    Bool,
    Integer,
    Float,
    Optional,
    StringList,
}

impl Shape {
    fn expected(self) -> &'static str {
        match self {
            Self::String => "a string or supported enum variant",
            Self::Bool => "true or false",
            Self::Integer => "an integer within the setting's range",
            Self::Float => "a finite number",
            Self::Optional => "a value of the setting's type (empty clears numeric/bool options)",
            Self::StringList => "a comma-separated list",
        }
    }
}

struct Override {
    key: String,
    pointer: String,
    shape: Shape,
}

/// Derive the fixed configuration surface from serde, including unset options.
/// JSON retains nulls and the full u64 range, which a TOML value would lose.
/// Empty maps are file-only namespaces; arrays of tables are file-only too.
static OVERRIDES: LazyLock<Vec<Override>> = LazyLock::new(|| {
    let defaults = serde_json::to_value(AppConfig::default()).expect("config defaults serialize");
    let mut overrides = Vec::new();
    collect_overrides(&defaults, &defaults, &mut Vec::new(), &mut overrides);
    overrides
});

fn collect_overrides(
    defaults: &Value,
    value: &Value,
    path: &mut Vec<String>,
    overrides: &mut Vec<Override>,
) {
    let pointer = path
        .iter()
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect::<String>();
    let mut shape = match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                path.push(name.clone());
                collect_overrides(defaults, value, path, overrides);
                path.pop();
            }
            return;
        }
        Value::Array(_) => {
            // Serde distinguishes Vec<String> from Vec<struct>, even when the
            // default is empty, without a second registry of field types.
            if !accepts(defaults, &pointer, serde_json::json!([""])) {
                return;
            }
            Shape::StringList
        }
        Value::Null => Shape::Optional,
        Value::String(_) => Shape::String,
        Value::Bool(_) => Shape::Bool,
        Value::Number(number) if number.is_f64() => Shape::Float,
        Value::Number(_) => Shape::Integer,
    };
    // Some options have a non-null default (e.g. admin_rps). Ask serde
    // whether null is allowed rather than losing their clearing semantics.
    if !matches!(shape, Shape::Optional | Shape::StringList)
        && accepts(defaults, &pointer, Value::Null)
    {
        shape = Shape::Optional;
    }
    overrides.push(Override {
        key: format!("ORION_{}", path.join("__").to_ascii_uppercase()),
        pointer,
        shape,
    });
}

fn accepts(defaults: &Value, pointer: &str, value: Value) -> bool {
    let mut probe = defaults.clone();
    *probe.pointer_mut(pointer).expect("default field exists") = value;
    serde_json::from_value::<AppConfig>(probe).is_ok()
}

/// Every fixed scalar or string-list setting, named from its serialized path.
/// The same catalogue drives overrides and the unknown-variable guard. Dynamic
/// maps and arrays of tables use config-file references instead of overrides.
pub fn known_env_override_keys() -> BTreeSet<String> {
    OVERRIDES.iter().map(|entry| entry.key.clone()).collect()
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

/// Apply overrides to a private copy and publish only after every value parses.
/// Serde remains the type authority: it checks numeric bounds, optional types
/// and enum variants instead of duplicating those declarations here.
pub(super) fn apply_env_overrides_with<F>(
    config: &mut AppConfig,
    env_var: F,
) -> Result<(), OrionError>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    let supplied: Vec<_> = OVERRIDES
        .iter()
        .filter_map(|entry| env_var(&entry.key).ok().map(|raw| (entry, raw)))
        .collect();
    if supplied.is_empty() {
        return Ok(());
    }
    // Vars are opaque TOML, including values with no lossless JSON form.
    // They have no env overrides; keep them outside the typed round trip.
    let mut scratch = config.clone();
    let vars = std::mem::take(&mut scratch.vars);
    let mut tree = serde_json::to_value(scratch).map_err(|_| OrionError::Config {
        message: "cannot serialize configuration for environment overrides".to_string(),
    })?;
    let mut parsed = None;
    for (entry, raw) in supplied {
        let next = candidates(entry.shape, &raw).into_iter().find_map(|value| {
            *tree
                .pointer_mut(&entry.pointer)
                .expect("config field exists") = value;
            serde_json::from_value::<AppConfig>(tree.clone()).ok()
        });
        parsed = Some(next.ok_or_else(|| OrionError::Config {
            // Neither raw input nor serde errors belong here: both can carry
            // credentials, including values from elsewhere in the config tree.
            message: format!(
                "{}: invalid value; expected {}",
                entry.key,
                entry.shape.expected()
            ),
        })?);
    }
    if let Some(mut parsed) = parsed {
        parsed.vars = vars;
        *config = parsed;
    }
    Ok(())
}

fn candidates(shape: Shape, raw: &str) -> Vec<Value> {
    let mut values = Vec::new();
    if matches!(shape, Shape::String | Shape::Optional) {
        // Strings stay literal, including empty and numeric-looking values.
        values.push(Value::String(raw.to_string()));
        let lower = raw.to_lowercase();
        if lower != raw {
            // Enum variants historically accept case-insensitive input.
            // Ordinary strings accept the first candidate unchanged.
            values.push(Value::String(lower));
        }
    }
    if matches!(shape, Shape::Optional) && raw.trim().is_empty() {
        // A string option already accepted ""; numeric/bool options clear.
        values.push(Value::Null);
    }
    if matches!(shape, Shape::Bool | Shape::Optional)
        && let Ok(value) = raw.parse::<bool>()
    {
        values.push(Value::Bool(value));
    }
    if matches!(shape, Shape::Integer | Shape::Optional) {
        if let Ok(value) = raw.parse::<i64>() {
            values.push(Value::from(value));
        } else if let Ok(value) = raw.parse::<u64>() {
            values.push(Value::from(value));
        }
    }
    if matches!(shape, Shape::Float | Shape::Optional)
        && let Ok(value) = raw.parse::<f64>()
        && let Some(value) = serde_json::Number::from_f64(value)
    {
        values.push(Value::Number(value));
    }
    if matches!(shape, Shape::StringList) {
        values.push(Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| Value::String(value.to_string()))
                .collect(),
        ));
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crate::config::LogFormat;

    fn make_env_reader<'a>(
        env: &'a HashMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> + 'a {
        move |key| {
            env.get(key)
                .map(|v: &&str| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    /// The serde-derived catalogue includes every supported leaf shape.
    #[test]
    fn known_keys_cover_every_override_shape() {
        let keys = known_env_override_keys();
        for expected in [
            // Scalars at three nesting depths.
            "ORION_ENVIRONMENT",
            "ORION_SERVER__PORT",
            "ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD",
            // Optional scalars and string lists.
            "ORION_SERVER__DOCS__ENABLED",
            "ORION_METRICS__BIND_ADDR",
            "ORION_CORS__ALLOWED_ORIGINS",
            // Enums.
            "ORION_LOGGING__FORMAT",
            "ORION_TRACE_STORAGE__MODE",
            "ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW",
        ] {
            assert!(keys.contains(expected), "{expected} is not in the key set");
        }
        assert!(
            keys.len() > 90,
            "only {} keys derived — the catalogue is missing \
             overrides, which would make the guard reject real settings",
            keys.len()
        );
        assert!(!keys.contains("ORION_"), "the bare prefix is not a key");
        for file_only in [
            "ORION_VARS",
            "ORION_SECRETS",
            "ORION_OAUTH2_LOGIN__PROVIDERS",
            "ORION_PLUGINS__OVERRIDES",
        ] {
            assert!(!keys.contains(file_only), "{file_only} is file-only");
        }
    }

    /// An unset environment consults the entire catalogue without changing config.
    #[test]
    fn unset_environment_consults_every_key_and_applies_nothing() {
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
        // Every key in the guard allowlist is actually consulted.
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
    fn connector_budget_env_overrides_are_typed_and_validated() {
        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, |key| match key {
            "ORION_ENGINE__CONNECTOR_BUDGET__MAX_ATTEMPTS" => Ok("80".to_string()),
            "ORION_ENGINE__CONNECTOR_BUDGET__MAX_BYTES" => Ok("33554432".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        })
        .expect("valid connector budget overrides");
        assert_eq!(config.engine.connector_budget.max_attempts, 80);
        assert_eq!(config.engine.connector_budget.max_bytes, 33554432);
        assert_eq!(config.engine.connector_budget.max_depth, 8);
        let error = apply_env_overrides_with(&mut config, |key| match key {
            "ORION_ENGINE__CONNECTOR_BUDGET__MAX_DEPTH" => Ok("4294967296".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        })
        .expect_err("depth must fit u32");
        assert!(error.to_string().contains("MAX_DEPTH"));
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
        env.insert("ORION_CLUSTER__ENABLED", "true");
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
        assert!(config.cluster.enabled);
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

    #[test]
    fn strings_stay_literal_and_optional_strings_do_not_clear() {
        for value in ["", "  ", "00123", "true", "MiXeD", "[1,2]"] {
            let env = HashMap::from([
                ("ORION_METRICS__BIND_ADDR", value),
                ("ORION_STORAGE__CONNECTOR_ENCRYPTION_KEY", value),
            ]);
            let mut config = AppConfig::default();
            apply_env_overrides_with(&mut config, make_env_reader(&env))
                .expect("test config parses");
            assert_eq!(config.metrics.bind_addr.as_deref(), Some(value));
            assert_eq!(config.storage.connector_encryption_key, value);
        }
    }

    #[test]
    fn optional_numbers_and_bools_can_be_set_then_cleared() {
        let mut config = AppConfig::default();
        let env = HashMap::from([
            ("ORION_SERVER__VERBOSE_ERRORS", "false"),
            ("ORION_STORAGE__BACKUP_RETENTION_COUNT", "7"),
            ("ORION_CORS__MAX_AGE_SECS", "18446744073709551615"),
        ]);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        assert_eq!(config.server.verbose_errors, Some(false));
        assert_eq!(config.storage.backup_retention_count, Some(7));
        assert_eq!(config.cors.max_age_secs, Some(u64::MAX));
        let env = HashMap::from([
            ("ORION_SERVER__VERBOSE_ERRORS", "  "),
            ("ORION_STORAGE__BACKUP_RETENTION_COUNT", ""),
            ("ORION_CORS__MAX_AGE_SECS", "\t"),
        ]);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        assert_eq!(config.server.verbose_errors, None);
        assert_eq!(config.storage.backup_retention_count, None);
        assert_eq!(config.cors.max_age_secs, None);
    }

    #[test]
    fn enums_remain_case_insensitive() {
        let env = HashMap::from([
            ("ORION_LOGGING__FORMAT", "JsOn"),
            ("ORION_TRACE_STORAGE__MODE", "ASYNC"),
            ("ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW", "BlOcK"),
        ]);
        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        let value = serde_json::to_value(config).expect("test config parses");
        assert_eq!(value["logging"]["format"], "json");
        assert_eq!(value["trace_storage"]["mode"], "async");
        assert_eq!(value["trace_storage"]["async_on_overflow"], "block");
    }

    #[test]
    fn numeric_bounds_and_strict_boolean_parsing_follow_field_types() {
        let env = HashMap::from([
            ("ORION_SERVER__PORT", "65535"),
            ("ORION_STORAGE__BUSY_TIMEOUT_MS", "18446744073709551615"),
            ("ORION_CRON__CLAIM_BATCH_SIZE", "-9223372036854775808"),
        ]);
        let mut config = AppConfig::default();
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        assert_eq!(config.server.port, u16::MAX);
        assert_eq!(config.storage.busy_timeout_ms, u64::MAX);
        assert_eq!(config.cron.claim_batch_size, i64::MIN);

        for (key, value) in [
            ("ORION_SERVER__PORT", "65536"),
            ("ORION_SERVER__PORT", "-1"),
            ("ORION_SERVER__PORT", "1.0"),
            ("ORION_SERVER__PORT", " 3000"),
            ("ORION_STORAGE__BUSY_TIMEOUT_MS", "18446744073709551616"),
            ("ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS", "4294967296"),
            ("ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS", "1e3"),
            ("ORION_METRICS__ENABLED", "TRUE"),
            ("ORION_SERVER__DOCS__ENABLED", "TRUE"),
            ("ORION_TRACING__SAMPLE_RATE", "NaN"),
        ] {
            let env = HashMap::from([(key, value)]);
            let err = apply_env_overrides_with(&mut config, make_env_reader(&env))
                .expect_err("invalid override must fail");
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn lists_trim_drop_empty_entries_and_allow_empty_lists() {
        let mut config = AppConfig::default();
        let env = HashMap::from([("ORION_PLUGINS__TRUST__PUBLIC_KEYS", " KeyA, , KeyB, ")]);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        assert_eq!(config.plugins.trust.public_keys, ["KeyA", "KeyB"]);
        let env = HashMap::from([("ORION_PLUGINS__TRUST__PUBLIC_KEYS", " , ")]);
        apply_env_overrides_with(&mut config, make_env_reader(&env)).expect("test config parses");
        assert!(config.plugins.trust.public_keys.is_empty());
    }

    #[test]
    fn invalid_override_is_atomic_and_does_not_echo_values() {
        let mut config = AppConfig::default();
        config
            .secrets
            .0
            .insert("token".into(), "private-token".into());
        let before = serde_json::to_value(&config).expect("test config parses");
        let env = HashMap::from([
            ("ORION_ADMIN_AUTH__ENABLED", "true"),
            ("ORION_SERVER__PORT", "private-input"),
        ]);
        let err = apply_env_overrides_with(&mut config, make_env_reader(&env))
            .expect_err("invalid override must fail");
        let message = err.to_string();
        assert!(message.contains("ORION_SERVER__PORT"));
        assert!(!message.contains("private-input"));
        assert!(!message.contains("private-token"));
        assert_eq!(
            serde_json::to_value(config).expect("config serializes"),
            before
        );
    }

    #[test]
    fn overrides_preserve_file_only_maps_tables_and_toml_datetimes() {
        let mut config: AppConfig = toml::from_str(
            r#"
[vars]
date = 2026-10-26T12:34:56Z
values = [1, 2, 3]
nested = { enabled = true, count = 9223372036854775807 }
[secrets]
token = "env://MY_TOKEN"
[oauth2_login.providers.work]
kind = "oidc"
client_secret = "private-value"
scopes = ["openid", "email"]
[[plugins.overrides]]
id = "org.example.plugin"
timeout_ms = 100
"#,
        )
        .expect("test config parses");
        let before = toml::Value::try_from(&config).expect("test config parses");
        let env = HashMap::from([("ORION_SERVER__PORT", "9090")]);
        let seen = RefCell::new(BTreeSet::new());
        let reader = make_env_reader(&env);
        apply_env_overrides_with(&mut config, |key| {
            seen.borrow_mut().insert(key.to_string());
            reader(key)
        })
        .expect("test config parses");
        assert_eq!(config.server.port, 9090);
        let after = toml::Value::try_from(&config).expect("test config parses");
        for section in ["vars", "secrets", "oauth2_login", "plugins"] {
            assert_eq!(before[section], after[section], "{section} changed");
        }
        // Configured map entries never enlarge the override allowlist.
        assert_eq!(seen.into_inner(), known_env_override_keys());
    }

    #[test]
    fn opaque_toml_numbers_survive_unrelated_overrides() {
        let mut config: AppConfig =
            toml::from_str("[vars]\nnan = nan\ninf = inf").expect("TOML accepts non-finite floats");
        let env = HashMap::from([("ORION_SERVER__PORT", "9090")]);
        apply_env_overrides_with(&mut config, make_env_reader(&env))
            .expect("an unrelated override preserves opaque TOML");
        assert!(config.vars.0["nan"].as_float().expect("float").is_nan());
        assert_eq!(config.vars.0["inf"].as_float(), Some(f64::INFINITY));
    }
}
