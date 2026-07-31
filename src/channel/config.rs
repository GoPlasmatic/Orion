use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-channel baseline configuration.
/// All fields are optional with sensible defaults.
///
/// `deny_unknown_fields` because every field here is a *guard*, and a key this
/// struct does not recognise is a guard that silently does not run: a stored
/// `"deduplicaton"` typo meant no idempotency, no error, forever. The channel's
/// stored `config_json` is the operator's original document — nothing
/// re-serialises it — so an unrecognised key survives every reload until
/// someone notices the behaviour is missing. Rejecting it turns that into a
/// create-time 400, or an F35 quarantine for a channel already stored: refused
/// at every ingress rather than served with a guard quietly absent. This is the
/// same posture the config file (`deny_unknown_fields` throughout), the
/// connector configs and both dialect envelopes (W5/W6) already take.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    /// The channel's server-side origin allow-list.
    ///
    /// N24: the pre-1.0 key was `cors: { allowed_origins: [...] }`, which
    /// promised CORS and delivered a rejection check. It set no
    /// `Access-Control-Allow-Origin` and answered no preflight — the router's
    /// platform CORS layer short-circuits a genuine preflight (`OPTIONS`
    /// carrying `Access-Control-Request-Method`) before a channel is even
    /// resolved. The control is real and worth keeping: it is the only
    /// *server-side* origin check, and it runs on every request that reaches
    /// the handler, since `[cors]` leaves a non-preflighted cross-origin
    /// request to run and merely omits the response header, and does nothing
    /// at all for a non-browser caller. Only the name was a lie, so the name
    /// is what changed.
    ///
    /// The old spelling is not accepted. Silently ignoring it would drop the
    /// check on every stored channel that used it — a security regression
    /// dressed as a rename — so `deny_unknown_fields` on this struct refuses
    /// the whole config instead, and the channel is quarantined rather than
    /// served without its allow-list. `orion-server preflight` names every
    /// stored channel still carrying it.
    pub fn allowed_origins(&self) -> Option<&[String]> {
        self.origin_allow_list.as_deref()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackpressureConfig {
    /// Maximum concurrent requests for this channel **on this node**.
    /// Excess requests are rejected immediately with 503 (no queueing).
    ///
    /// N9: named for what it bounds — the semaphore is per process, so N
    /// replicas admit up to N× this value in total. The pre-1.0 name
    /// `max_concurrent` read as an absolute cluster-wide cap while sitting
    /// next to dedup/rate-limit controls that *are* shared in cluster mode.
    /// It is not accepted: the field has no `serde(default)`, so a stored
    /// config using the old spelling fails with `missing field
    /// max_concurrent_per_node` and the channel is quarantined — a channel
    /// admitted N× its intended concurrency is not a quiet outcome worth
    /// having.
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

    /// N9: the pre-1.0 spelling is refused, not silently accepted. Failing to
    /// parse quarantines the channel; accepting it under a name that means
    /// something else would admit N× the intended concurrency.
    #[test]
    fn test_backpressure_old_name_is_refused() {
        let err =
            serde_json::from_str::<ChannelConfig>(r#"{"backpressure": {"max_concurrent": 7}}"#)
                .expect_err("the pre-1.0 `max_concurrent` spelling must not parse");
        let message = err.to_string();
        assert!(
            message.contains("max_concurrent"),
            "the error must name the offending key: {message}"
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

    /// N24: `origin_allow_list` is the only spelling.
    #[test]
    fn test_origin_allow_list() {
        let new_key: ChannelConfig =
            serde_json::from_str(r#"{"origin_allow_list": ["https://app.example.com"]}"#)
                .expect("test");
        assert_eq!(
            new_key.allowed_origins(),
            Some(["https://app.example.com".to_string()].as_slice())
        );

        // No list at all means the channel checks nothing.
        assert!(ChannelConfig::default().allowed_origins().is_none());
    }

    /// N24: the pre-1.0 `cors` spelling is *refused*, not ignored. Parsing it
    /// and dropping the key would leave every channel that used it serving
    /// with no origin check — the security regression the rename was written
    /// to avoid. `deny_unknown_fields` makes the whole config fail instead,
    /// which quarantines the channel.
    #[test]
    fn test_pre_1_0_cors_spelling_is_refused_not_ignored() {
        for stored in [
            r#"{"cors": {"allowed_origins": ["https://old.example.com"]}}"#,
            r#"{"cors": {}}"#,
        ] {
            let err = serde_json::from_str::<ChannelConfig>(stored)
                .expect_err("the pre-1.0 `cors` spelling must not parse");
            let message = err.to_string();
            assert!(
                message.contains("cors"),
                "the error must name the offending key: {message}"
            );
        }
    }

    /// The general case the `cors` removal relies on: an unrecognised key is a
    /// guard that would silently not run, so it fails the whole config.
    #[test]
    fn test_unknown_channel_config_key_is_refused() {
        let err = serde_json::from_str::<ChannelConfig>(
            r#"{"deduplicaton": {"header": "Idempotency-Key"}}"#,
        )
        .expect_err("a misspelled guard key must not be silently ignored");
        assert!(
            err.to_string().contains("deduplicaton"),
            "the error must name the typo: {err}"
        );
    }

    #[test]
    fn test_channel_config_empty_json() {
        let config: ChannelConfig = serde_json::from_str("{}").expect("test");
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
    }
}
