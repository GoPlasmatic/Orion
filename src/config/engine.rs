use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

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
    /// Maximum entries in the shared in-memory cache (default dedup store,
    /// default response cache, and every `backend = "memory"` cache
    /// connector). Least-recently-used entries are evicted on insert once
    /// the bound is reached. `0` disables the bound — entries written
    /// without a TTL are never reclaimed, so only do that when the key set
    /// is known to be finite.
    pub max_memory_cache_entries: usize,
    /// Header whose value identifies the caller for sticky canary-rollout
    /// bucketing (e.g. "x-user-id"). Empty (default): fall back to the
    /// forwarded client IP (`x-forwarded-for` / `x-real-ip`); with neither,
    /// the bucket is random per request.
    pub rollout_sticky_header: String,
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
            max_memory_cache_entries: 100_000,
            rollout_sticky_header: String::new(),
        }
    }
}

impl EngineConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(
            u64::from(self.max_channel_call_depth),
            "engine.max_channel_call_depth",
        )?;
        require_nonzero(
            self.default_channel_call_timeout_ms,
            "engine.default_channel_call_timeout_ms",
        )?;
        require_nonzero(
            self.health_check_timeout_secs,
            "engine.health_check_timeout_secs",
        )?;
        require_nonzero(self.reload_timeout_secs, "engine.reload_timeout_secs")?;
        Ok(())
    }
}
