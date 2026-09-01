mod admin_auth;
mod cluster;
mod engine;
mod env_overrides;
pub(crate) mod env_substitute;
mod jwt;
mod kafka;
mod logging;
mod observability;
mod query;
mod rate_limit;
mod retired_env;
mod server;
mod storage;
mod trace_queue;
mod unknown_env;
pub(super) mod validation;
pub mod vars;
mod write;

// Re-export all types so `use crate::config::Foo` keeps working.
pub use admin_auth::{AdminAuthConfig, constant_time_eq};
pub use cluster::ClusterConfig;
pub use engine::EngineConfig;
pub use env_overrides::known_env_override_keys;
pub use jwt::JwtConfig;
pub use kafka::{DlqConfig, KafkaAuthConfig, KafkaIngestConfig, TopicMapping};
pub use logging::{LogFormat, LoggingConfig};
pub use observability::{
    AsyncOnOverflow, CorsConfig, MetricsConfig, TraceStorageConfig, TraceStorageMode, TracingConfig,
};
pub use query::QueryConfig;
pub use rate_limit::{EndpointRateLimits, RateLimitConfig};
pub use retired_env::retired_env_names;
pub use server::{CompressionConfig, DocsConfig, IngestConfig, ServerConfig, TlsConfig};
pub use storage::StorageConfig;
pub use trace_queue::TraceQueueConfig;
pub use unknown_env::{RESERVED_PREFIX as RESERVED_ENV_PREFIX, looks_like_env_override};
pub use vars::{SecretsConfig, VarsConfig};
pub use write::WriteConfig;

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::errors::OrionError;

/// Top-level application configuration.
///
/// `Default` is implemented by hand rather than derived so that it agrees with
/// the `#[serde(default = "…")]` attributes below. A derived `Default` would
/// give `environment = ""` while a config file declaring no `environment` key
/// gives `"development"`, making "the default" depend on how the config was
/// produced (F36). `config_docs_drift_test` asserts the two agree for every
/// setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    /// Deployment environment (e.g. "development", "production").
    /// Controls safety checks like CORS wildcard rejection.
    /// Override via `ORION_ENVIRONMENT`.
    #[serde(default = "default_environment")]
    pub environment: String,
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ingest: IngestConfig,
    pub engine: EngineConfig,
    pub trace_queue: TraceQueueConfig,
    pub query: QueryConfig,
    pub write: WriteConfig,
    pub kafka: KafkaIngestConfig,
    pub logging: LoggingConfig,
    pub metrics: MetricsConfig,
    pub cors: CorsConfig,
    pub tracing: TracingConfig,
    pub trace_storage: TraceStorageConfig,
    pub rate_limit: RateLimitConfig,
    pub channel_filter: ChannelFilterConfig,
    pub audit: AuditConfig,
    pub admin_auth: AdminAuthConfig,
    /// Instance-wide JWT verification policy (the JWKS egress rule).
    pub jwt: JwtConfig,
    pub cluster: ClusterConfig,
    /// Deployment values stamped into `metadata.vars` on every message, and
    /// therefore visible in traces. Free-form, so no env override — `${VAR}`
    /// in the value covers that.
    pub vars: VarsConfig,
    /// Secret references resolved at startup and published to the engine,
    /// where `{"secret": "name"}` reaches them. Never part of a message, so
    /// never part of a trace.
    pub secrets: SecretsConfig,
}

fn default_environment() -> String {
    "development".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            environment: default_environment(),
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            ingest: IngestConfig::default(),
            engine: EngineConfig::default(),
            trace_queue: TraceQueueConfig::default(),
            query: QueryConfig::default(),
            write: WriteConfig::default(),
            kafka: KafkaIngestConfig::default(),
            logging: LoggingConfig::default(),
            metrics: MetricsConfig::default(),
            cors: CorsConfig::default(),
            tracing: TracingConfig::default(),
            trace_storage: TraceStorageConfig::default(),
            rate_limit: RateLimitConfig::default(),
            channel_filter: ChannelFilterConfig::default(),
            audit: AuditConfig::default(),
            admin_auth: AdminAuthConfig::default(),
            jwt: JwtConfig::default(),
            cluster: ClusterConfig::default(),
            vars: VarsConfig::default(),
            secrets: SecretsConfig::default(),
        }
    }
}

impl AppConfig {
    /// Returns true when the environment is a production variant.
    pub fn is_production(&self) -> bool {
        self.environment.to_lowercase().starts_with("prod")
    }

    /// Whether `/docs` and `/api/v1/openapi.json` are served (S17).
    ///
    /// `server.docs.enabled` when set; otherwise enabled exactly when the
    /// environment is not a production variant — the same prefix rule that
    /// turns the admin-auth and CORS-wildcard checks fatal.
    pub fn docs_enabled(&self) -> bool {
        self.server
            .docs
            .enabled
            .unwrap_or_else(|| !self.is_production())
    }

    /// Whether the data plane returns real task-failure messages instead of the
    /// generic placeholder.
    ///
    /// `server.verbose_errors` when set; otherwise on exactly when the
    /// environment is not a production variant — the same prefix rule as
    /// [`AppConfig::docs_enabled`]. `validate` refuses an explicit `true` in
    /// production, so the `true` branch here is only ever reached outside it.
    pub fn verbose_errors(&self) -> bool {
        self.server
            .verbose_errors
            .unwrap_or_else(|| !self.is_production())
    }
}

/// Selects which channels an Orion instance loads from the database. Named
/// `[channel_filter]` rather than `[channels]` (C22) because it configures the
/// *selection*, not the channels themselves — those live in the database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelFilterConfig {
    /// Glob patterns for channels to include. Empty means include all.
    pub include: Vec<String>,
    /// Glob patterns for channels to exclude. Applied after include.
    pub exclude: Vec<String>,
}

/// Admin audit-log retention. Split out of `[queue]` (C22): audit rows have
/// nothing to do with the async trace queue, and their cleanup job used to
/// borrow the trace job's cadence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    /// How long to retain admin audit-log entries in days (0 = forever).
    /// Audit rows are written by every admin mutation and are never otherwise
    /// removed, so leaving this at 0 makes `audit_logs` grow without bound.
    pub retention_days: u64,
    /// How often the audit-log cleanup task runs, in seconds.
    pub cleanup_interval_secs: u64,
    /// Bound on audit rows accepted but not yet written (O7). Admin mutations
    /// never wait on the INSERT; this caps how far behind the writer may fall
    /// before submissions are dropped and counted in
    /// `orion_audit_events_dropped_total{reason="queue_full"}`.
    pub max_pending: usize,
    /// How long shutdown waits for the audit queue to drain, in seconds.
    /// A database that has stopped accepting writes must not hold the process
    /// open, so the drain gives up after this and logs how many rows it
    /// abandoned. Must be non-zero — `0` is not "no bound" here, it would
    /// elapse on the first poll and skip the drain entirely, which is the
    /// defect O7 exists to fix.
    pub drain_timeout_secs: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            cleanup_interval_secs: 3600,
            max_pending: 1000,
            drain_timeout_secs: 5,
        }
    }
}

impl AuditConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        validation::require_nonzero(self.max_pending as u64, "audit.max_pending")?;
        // Elsewhere 0 is a "disabled" sentinel (`server.shutdown_force_timeout_secs`,
        // `kafka.lag_poll_interval_secs`), so an operator could reasonably read
        // it as "wait forever". It is the opposite: `tokio::time::timeout`
        // elapses on the first poll, so the drain is skipped and every clean
        // shutdown logs a drain failure with `lost = 0`.
        validation::require_nonzero(self.drain_timeout_secs, "audit.drain_timeout_secs")
    }
}

/// Load configuration from an optional TOML file path, then apply env overrides.
///
/// `${VAR}` and `${VAR:-default}` placeholders in the TOML file are
/// substituted from the process environment before TOML parsing. This
/// lets secrets stay out of the config file without forcing every value
/// to be redeclared as an `ORION_*` env var. See `env_substitute` for
/// the full grammar (including the `$$` literal-dollar escape).
///
/// The names those placeholders resolve are carried into the override step:
/// they are variables Orion reads on the file's behalf, so the
/// unknown-`ORION_*`-variable guard (C4d) must not refuse them.
pub fn load_config(path: Option<&str>) -> Result<AppConfig, OrionError> {
    let mut referenced_by_config_file = std::collections::BTreeSet::new();
    let mut config = if let Some(p) = path {
        let raw = std::fs::read_to_string(Path::new(p)).map_err(|e| OrionError::Internal {
            context: format!("Failed to read config file '{p}'"),
            source: Some(Box::new(e)),
        })?;
        referenced_by_config_file = env_substitute::referenced_vars(&raw);
        let content = env_substitute::substitute(&raw, p)?;
        toml::from_str::<AppConfig>(&content).map_err(|e| OrionError::Internal {
            context: format!("Failed to parse config file '{p}'"),
            source: Some(Box::new(e)),
        })?
    } else {
        AppConfig::default()
    };

    env_overrides::apply_env_overrides(&mut config, &referenced_by_config_file)?;
    validation::validate_config(&config)?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the process environment.
    ///
    /// `load_config` now *scans* the environment (C4d), so a test that sets an
    /// `ORION_*` variable is visible to any other test loading a config at the
    /// same moment — and `std::env::set_var` is `unsafe` in Rust 2024 for the
    /// same reason. Every test below that either sets a variable or calls
    /// `load_config` takes this lock first.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
        assert_eq!(config.trace_queue.shutdown_timeout_secs, 30);
    }

    #[test]
    fn test_load_config_no_file() {
        let _guard = env_guard();
        let config = load_config(None).expect("test");
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
        let config: AppConfig = toml::from_str(toml_str).expect("test");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.storage.url, "sqlite:test.db");
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let _guard = env_guard();
        let result = load_config(Some("/nonexistent/path/config.toml"));
        assert!(result.is_err());
    }

    // -- C4d: unknown keys are a hard error in the *environment* too --
    //
    // C4b covered the file. Overrides are matched by name, so nothing
    // deserialises them and a misspelled variable was simply never asked for:
    // `ORION_SERVER__PORTT=3000` did nothing, silently. The mechanism is unit
    // tested in `unknown_env`; these two go through the real entry point,
    // which is where the process environment is actually scanned.

    /// A typo'd override refuses the whole load rather than being ignored.
    #[test]
    fn load_config_refuses_a_misspelled_override() {
        let _guard = env_guard();
        // SAFETY: the env lock is held, so no other test in this binary is
        // reading or writing the environment concurrently.
        unsafe { std::env::set_var("ORION_SERVER__PORTT", "3000") };
        let result = load_config(None);
        unsafe { std::env::remove_var("ORION_SERVER__PORTT") };

        let err = result
            .expect_err("a misspelled ORION_* variable must not be silently ignored")
            .to_string();
        assert!(err.contains("ORION_SERVER__PORTT"), "{err}");
        assert!(err.contains("ORION_SERVER__PORT"), "{err}");
    }

    /// The reserved namespace is the escape hatch for variables Orion cannot
    /// enumerate — `env://` connector secrets live in the database.
    #[test]
    fn load_config_ignores_the_reserved_namespace() {
        let _guard = env_guard();
        // SAFETY: as above — the env lock is held for the whole test.
        unsafe { std::env::set_var("ORION_SECRET_SOME_TOKEN", "s3cret") };
        let result = load_config(None);
        unsafe { std::env::remove_var("ORION_SECRET_SOME_TOKEN") };
        result.expect("ORION_SECRET_* is never interpreted as configuration");
    }

    // -- C4b: unknown keys are a hard error, not a silent default --
    //
    // Every config struct is `#[serde(default)]`, so before 1.0 a misspelled
    // key simply did not deserialize and the field kept its default. There is
    // no effective-config dump to notice from, which made a typo in a
    // *security* setting fail open and silent. These tests pin the three
    // shapes a typo actually takes.

    /// A misspelled key inside a real section must name the offending key.
    #[test]
    fn misspelled_key_is_rejected() {
        let err = toml::from_str::<AppConfig>("[server]\nwrokers = 4\n")
            .expect_err("a typo'd key must not deserialize to the default");
        let msg = err.to_string();
        assert!(
            msg.contains("wrokers"),
            "the error must name the unknown key so it can be found, got: {msg}"
        );
    }

    /// A misspelled *section* is caught by `deny_unknown_fields` on AppConfig
    /// itself — otherwise an entire block of settings silently does nothing.
    #[test]
    fn misspelled_section_is_rejected() {
        let err = toml::from_str::<AppConfig>("[serverr]\nport = 3000\n")
            .expect_err("a typo'd section must not be ignored");
        assert!(err.to_string().contains("serverr"));
    }

    /// The case that motivated this: a typo in an auth setting used to leave
    /// the guard at its default while the operator believed it was set.
    #[test]
    fn misspelled_security_key_is_rejected() {
        let err = toml::from_str::<AppConfig>("[admin_auth]\nenable = true\n")
            .expect_err("`enable` is not `enabled` and must not silently disable admin auth");
        assert!(err.to_string().contains("enable"));
    }

    /// Nested sections carry the attribute too, not just top-level ones.
    #[test]
    fn misspelled_nested_key_is_rejected() {
        let err = toml::from_str::<AppConfig>("[server.tls]\nenabld = true\n")
            .expect_err("nested structs must reject unknown keys as well");
        assert!(err.to_string().contains("enabld"));
    }

    /// The flip side: everything the shipped example documents must still
    /// load. `deny_unknown_fields` is only safe if the docs are accurate.
    #[test]
    fn documented_keys_still_parse() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 3000

[server.tls]
enabled = false

[admin_auth]
enabled = true
api_keys = ["0123456789abcdef0123456789abcdef"]

[rate_limit]
enabled = true

[rate_limit.endpoints]
admin_rps = 20
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("documented keys must parse");
        assert_eq!(config.server.port, 3000);
        assert!(config.admin_auth.enabled);
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
        std::fs::write(&path, content).expect("test");
        path.to_string_lossy().into_owned()
    }

    /// Doubles as the C4d case for placeholders: `ORION_TEST_SUBST_DB_URL` is
    /// an `ORION_*` name that is not an override, and the load succeeds only
    /// because the config file references it.
    #[test]
    fn test_load_config_substitutes_env_vars() {
        let _guard = env_guard();
        let var_name = "ORION_TEST_SUBST_DB_URL";
        // SAFETY: the env lock is held, so no other test in this binary is
        // reading or writing the environment concurrently.
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
        let config = load_config(Some(&path)).expect("test");
        assert_eq!(config.storage.url, "postgres://test-host/db");
        unsafe {
            std::env::remove_var(var_name);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_uses_default_when_var_missing() {
        let _guard = env_guard();
        let toml = r#"
[server]
port = 8080

[storage]
url = "${ORION_TEST_NEVER_SET_VAR:-sqlite:fallback.db}"
"#;
        let path = write_temp_toml(toml, "default");
        let config = load_config(Some(&path)).expect("test");
        assert_eq!(config.storage.url, "sqlite:fallback.db");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_fails_on_missing_required_var() {
        let _guard = env_guard();
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
            other => unreachable!("expected Config error, got {other:?}"),
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
        let config: AppConfig = toml::from_str(toml_str).expect("test");
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

    /// Both audit knobs are load-bearing and neither has a meaningful zero:
    /// `max_pending = 0` is a queue that accepts nothing, and
    /// `drain_timeout_secs = 0` silently skips the drain O7 exists to add.
    #[test]
    fn audit_queue_knobs_reject_zero() {
        assert!(AuditConfig::default().validate().is_ok());
        let err = AuditConfig {
            max_pending: 0,
            ..AuditConfig::default()
        }
        .validate()
        .expect_err("a zero-capacity audit queue must be refused");
        assert!(err.to_string().contains("audit.max_pending"), "{err}");

        let err = AuditConfig {
            drain_timeout_secs: 0,
            ..AuditConfig::default()
        }
        .validate()
        .expect_err("a zero drain timeout skips the drain, it does not disable the bound");
        assert!(
            err.to_string().contains("audit.drain_timeout_secs"),
            "{err}"
        );
    }

    #[test]
    fn test_toml_parsing_admin_auth() {
        let toml_str = r#"
[admin_auth]
enabled = true
api_keys = ["my-key"]
header = "X-Custom-Auth"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("test");
        assert!(config.admin_auth.enabled);
        assert_eq!(config.admin_auth.api_keys, vec!["my-key".to_string()]);
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
        let config: AppConfig = toml::from_str(toml_str).expect("test");
        assert!(config.admin_auth.enabled);
        assert_eq!(
            config.admin_auth.api_keys,
            vec!["key-a".to_string(), "key-b".to_string()]
        );
    }
}
