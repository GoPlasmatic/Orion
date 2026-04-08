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

    /// CORS configuration for this channel.
    #[serde(default)]
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
pub struct ChannelCorsConfig {
    /// Allowed origins. Empty or absent means use platform default.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed HTTP methods.
    #[serde(default)]
    pub allowed_methods: Option<Vec<String>>,
    /// Allowed headers.
    #[serde(default)]
    pub allowed_headers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureConfig {
    /// Maximum concurrent requests for this channel.
    pub max_concurrent: usize,
    /// Optional queue depth before shedding load.
    #[serde(default)]
    pub queue_depth: Option<usize>,
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
            "backpressure": { "max_concurrent": 200 },
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }"#;
        let config: ChannelConfig = serde_json::from_str(json).unwrap();
        let rl = config.rate_limit.unwrap();
        assert_eq!(rl.requests_per_second, 100);
        assert_eq!(rl.burst, Some(20));
        assert!(rl.key_logic.is_some());
        assert_eq!(config.timeout_ms, Some(5000));
        let bp = config.backpressure.unwrap();
        assert_eq!(bp.max_concurrent, 200);
        let dedup = config.deduplication.unwrap();
        assert_eq!(dedup.header, "Idempotency-Key");
        assert_eq!(dedup.window_secs, Some(300));
    }

    #[test]
    fn test_channel_config_empty_json() {
        let config: ChannelConfig = serde_json::from_str("{}").unwrap();
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
    }
}
