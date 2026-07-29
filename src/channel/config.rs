use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-channel baseline configuration.
/// All fields are optional with sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<ChannelRateLimitConfig>,

    /// Maximum workflow execution time in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Response caching configuration.
    /// When enabled, sync responses are cached using the configured (or default
    /// in-memory) cache backend. Cache key is derived from channel name +
    /// request data hash (optionally scoped to `cache_key_fields`).
    #[serde(default)]
    pub cache: Option<ChannelCacheConfig>,

    /// Server-side allow-list of `Origin` header values for this channel.
    /// A request whose `Origin` is present and unlisted is refused `403`;
    /// `"*"` in the list allows any origin, and an absent list checks
    /// nothing. See [`ChannelConfig::allowed_origins`] for why this is not
    /// spelled `cors` any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_allow_list: Option<Vec<String>>,

    /// Deprecated spelling of [`ChannelConfig::origin_allow_list`]:
    /// `{"cors": {"allowed_origins": [...]}}`. Still parsed so channels
    /// stored before 1.0 keep working; prefer the new key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<ChannelCorsConfig>,

    /// Backpressure / load-shedding configuration.
    #[serde(default)]
    pub backpressure: Option<BackpressureConfig>,

    /// Request deduplication configuration.
    /// Extracts an idempotency key from the configured header and rejects
    /// duplicate submissions within the time window with 409 Conflict.
    #[serde(default)]
    pub deduplication: Option<DeduplicationConfig>,

    /// JSONLogic expression for input validation at the channel boundary.
    /// Evaluated against the request data. Returns truthy = pass, falsy = 400 reject.
    /// Example: `{ "and": [{ "!!": { "var": "data.order_id" } }, { ">": [{ "var": "data.quantity" }, 0] }] }`
    #[serde(default)]
    pub validation_logic: Option<Value>,

    /// Per-channel override of `[trace_storage]`. Each field is independently
    /// optional; unset fields fall back to the global setting.
    #[serde(default)]
    pub tracing: Option<ChannelTracingConfig>,
}

impl ChannelConfig {
    /// The effective origin allow-list: [`ChannelConfig::origin_allow_list`],
    /// falling back to the deprecated `cors.allowed_origins` spelling.
    ///
    /// N24: the old key promised CORS and delivered a rejection check. It set
    /// no `Access-Control-Allow-Origin` and answered no preflight — the
    /// router's platform CORS layer short-circuits a genuine preflight
    /// (`OPTIONS` carrying `Access-Control-Request-Method`) before a channel
    /// is even resolved. The control is real and worth keeping: it is the
    /// only *server-side* origin check, and it runs on every request that
    /// reaches the handler, since `[cors]` leaves a non-preflighted
    /// cross-origin request to run and merely omits the response header, and
    /// does nothing at all for a non-browser caller. Only the name was a lie,
    /// so the name is what changed.
    pub fn allowed_origins(&self) -> Option<&[String]> {
        self.origin_allow_list.as_deref().or_else(|| {
            self.cors
                .as_ref()
                .and_then(|c| c.allowed_origins.as_deref())
        })
    }
}

/// Per-channel override for the trace-storage policy. Each field overrides
/// the corresponding global value when set; otherwise the global default
/// applies. The resolved `EffectiveTraceConfig` lives on `ChannelRuntimeConfig`
/// so the request path doesn't merge per request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelTracingConfig {
    #[serde(default)]
    pub mode: Option<crate::config::TraceStorageMode>,
    #[serde(default)]
    pub sample_rate: Option<f64>,
    #[serde(default)]
    pub errors_only: Option<bool>,
    /// When `true`, the engine captures a per-task execution trace
    /// (intermediate input/output snapshots from `dataflow_rs::ExecutionTrace`)
    /// and persists it to the `task_trace_json` column. Off by default
    /// because each persisted trace grows proportional to message size
    /// times task count — only enable for debugging.
    #[serde(default)]
    pub task_details: Option<bool>,
}

/// What a guard does when its backing store cannot answer (N7).
///
/// Applies to the shared-Redis rate-limit window and to Redis-backed dedup
/// stores: a backend outage forces a choice between availability and
/// enforcement. `allow` (the default) keeps serving without the guard;
/// `deny` refuses the request with `503` — the right trade for
/// payment/idempotency workloads where a duplicate is worse than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendErrorPolicy {
    /// Fail open: the request proceeds as if the guard had passed.
    #[default]
    Allow,
    /// Fail closed: the request is refused with `503 Service Unavailable`.
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRateLimitConfig {
    /// Maximum requests per second.
    pub requests_per_second: u32,
    /// Burst allowance above the steady rate.
    #[serde(default)]
    pub burst: Option<u32>,
    /// JSONLogic expression to compute the rate limit key from request context.
    /// Context: `{ "client_ip": "...", "channel": "...", "headers": { ... } }`
    /// Default (absent): uses `client_ip` as the key.
    /// Example: `{ "var": "headers.x-api-key" }` for per-API-key limiting.
    /// Example: `{ "cat": [{ "var": "client_ip" }, ":", { "var": "headers.x-tenant-id" }] }`
    #[serde(default)]
    pub key_logic: Option<Value>,
    /// Policy when the rate-limit backend (the shared cluster Redis) cannot
    /// answer. Irrelevant to the in-process limiter, which cannot fail.
    #[serde(default)]
    pub on_backend_error: BackendErrorPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCacheConfig {
    /// Whether caching is enabled.
    pub enabled: bool,
    /// Cache TTL in seconds.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// Fields used to compute the cache key.
    #[serde(default)]
    pub cache_key_fields: Option<Vec<String>>,
    /// Optional cache connector name for the response cache backend.
    #[serde(default)]
    pub connector: Option<String>,
}

/// Deprecated `cors` block, kept so channels stored before 1.0 still parse.
/// Read through [`ChannelConfig::allowed_origins`], never directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCorsConfig {
    /// Allowed origins. Absent means the channel checks nothing.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureConfig {
    /// Maximum concurrent requests for this channel **on this node**.
    /// Excess requests are rejected immediately with 503 (no queueing).
    ///
    /// N9: named for what it bounds — the semaphore is per process, so N
    /// replicas admit up to N× this value in total. The old name
    /// `max_concurrent` read as an absolute cluster-wide cap while sitting
    /// next to dedup/rate-limit controls that *are* shared in cluster mode;
    /// it is accepted as an alias for one release.
    #[serde(alias = "max_concurrent")]
    pub max_concurrent_per_node: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeduplicationConfig {
    /// Header name containing the idempotency key.
    pub header: String,
    /// Time window in seconds for deduplication.
    #[serde(default)]
    pub window_secs: Option<u64>,
    /// Optional cache connector name for the dedup backend.
    /// When set, uses the connector's backend (redis or memory).
    /// When absent, uses the built-in in-memory store.
    #[serde(default)]
    pub connector: Option<String>,
    /// Policy when the dedup backend cannot answer: without it, an outage
    /// silently disables idempotency (every request treated as new).
    #[serde(default)]
    pub on_backend_error: BackendErrorPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_config_default() {
        let config = ChannelConfig::default();
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
        assert!(config.cache.is_none());
        assert!(config.backpressure.is_none());
        assert!(config.deduplication.is_none());
        assert!(config.validation_logic.is_none());
    }

    #[test]
    fn test_channel_config_deserialization() {
        let json = r#"{
            "rate_limit": { "requests_per_second": 100, "burst": 20, "key_logic": { "var": "client_ip" } },
            "timeout_ms": 5000,
            "backpressure": { "max_concurrent_per_node": 200 },
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }"#;
        let config: ChannelConfig = serde_json::from_str(json).expect("test");
        let rl = config.rate_limit.expect("test");
        assert_eq!(rl.requests_per_second, 100);
        assert_eq!(rl.burst, Some(20));
        assert!(rl.key_logic.is_some());
        assert_eq!(rl.on_backend_error, BackendErrorPolicy::Allow);
        assert_eq!(config.timeout_ms, Some(5000));
        let bp = config.backpressure.expect("test");
        assert_eq!(bp.max_concurrent_per_node, 200);
        let dedup = config.deduplication.expect("test");
        assert_eq!(dedup.header, "Idempotency-Key");
        assert_eq!(dedup.window_secs, Some(300));
        assert_eq!(dedup.on_backend_error, BackendErrorPolicy::Allow);
    }

    /// N9: configs stored before the rename keep working for one release.
    #[test]
    fn test_backpressure_old_name_is_an_alias() {
        let config: ChannelConfig =
            serde_json::from_str(r#"{"backpressure": {"max_concurrent": 7}}"#).expect("test");
        assert_eq!(
            config.backpressure.expect("test").max_concurrent_per_node,
            7
        );
    }

    /// N7: `on_backend_error` parses on both guard blocks; unknown values fail.
    #[test]
    fn test_on_backend_error_deserialization() {
        let json = r#"{
            "rate_limit": { "requests_per_second": 5, "on_backend_error": "deny" },
            "deduplication": { "header": "idem", "on_backend_error": "deny" }
        }"#;
        let config: ChannelConfig = serde_json::from_str(json).expect("test");
        assert_eq!(
            config.rate_limit.expect("test").on_backend_error,
            BackendErrorPolicy::Deny
        );
        assert_eq!(
            config.deduplication.expect("test").on_backend_error,
            BackendErrorPolicy::Deny
        );
        assert!(
            serde_json::from_str::<ChannelConfig>(
                r#"{"deduplication": {"header": "idem", "on_backend_error": "explode"}}"#
            )
            .is_err(),
            "unknown policy values must be rejected, not defaulted"
        );
    }

    /// N24: `origin_allow_list` is the key now, and the pre-1.0
    /// `cors.allowed_origins` spelling still resolves to it — a rename that
    /// silently dropped the check on every stored channel would be a
    /// security regression, not a documentation change.
    #[test]
    fn test_origin_allow_list_accepts_both_spellings() {
        let new_key: ChannelConfig =
            serde_json::from_str(r#"{"origin_allow_list": ["https://app.example.com"]}"#)
                .expect("test");
        assert_eq!(
            new_key.allowed_origins(),
            Some(["https://app.example.com".to_string()].as_slice())
        );

        let old_key: ChannelConfig =
            serde_json::from_str(r#"{"cors": {"allowed_origins": ["https://old.example.com"]}}"#)
                .expect("test");
        assert_eq!(
            old_key.allowed_origins(),
            Some(["https://old.example.com".to_string()].as_slice())
        );

        // The new key wins when a config carries both.
        let both: ChannelConfig = serde_json::from_str(
            r#"{"origin_allow_list": ["https://new.example.com"],
                "cors": {"allowed_origins": ["https://old.example.com"]}}"#,
        )
        .expect("test");
        assert_eq!(
            both.allowed_origins(),
            Some(["https://new.example.com".to_string()].as_slice())
        );

        // No list at all means the channel checks nothing.
        assert!(ChannelConfig::default().allowed_origins().is_none());
        let empty_cors: ChannelConfig = serde_json::from_str(r#"{"cors": {}}"#).expect("test");
        assert!(empty_cors.allowed_origins().is_none());
    }

    #[test]
    fn test_channel_config_empty_json() {
        let config: ChannelConfig = serde_json::from_str("{}").expect("test");
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
    }
}
