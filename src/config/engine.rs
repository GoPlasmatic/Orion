use serde::{Deserialize, Serialize};

/// Engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub circuit_breaker: crate::connector::circuit_breaker::CircuitBreakerConfig,
    /// Timeout in seconds for acquiring engine read lock in health checks.
    pub health_check_timeout_secs: u64,
    /// Timeout in seconds for acquiring engine write lock during reload.
    pub reload_timeout_secs: u64,
    /// Maximum nesting depth for channel_call invocations.
    pub max_channel_call_depth: u32,
    /// Default timeout in milliseconds for channel_call invocations.
    pub default_channel_call_timeout_ms: u64,
    /// Global default timeout in seconds for all outbound HTTP requests (safety net).
    /// Individual connector/task timeouts override this when shorter.
    pub global_http_timeout_secs: u64,
    /// Maximum entries in each external connector pool cache.
    /// LRU eviction removes the least-recently-used pool when exceeded.
    pub max_pool_cache_entries: usize,
    /// Interval in seconds between cache cleanup sweeps that evict expired entries.
    pub cache_cleanup_interval_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            circuit_breaker: Default::default(),
            health_check_timeout_secs: 2,
            reload_timeout_secs: 10,
            max_channel_call_depth: 10,
            default_channel_call_timeout_ms: 30_000,
            global_http_timeout_secs: 30,
            max_pool_cache_entries: 100,
            cache_cleanup_interval_secs: 60,
        }
    }
}
