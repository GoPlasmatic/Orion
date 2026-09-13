use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// Engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub circuit_breaker: crate::connector::circuit_breaker::CircuitBreakerConfig,
    /// Timeout in seconds for the `/readyz` cluster-Redis ping.
    pub health_check_timeout_secs: u64,
    /// Maximum nesting depth for channel_call invocations.
    pub max_channel_call_depth: u32,
    /// Default timeout in milliseconds for channel_call invocations.
    pub default_channel_call_timeout_ms: u64,
    /// Ceiling on a workflow `loop`'s `max`, refused at write time.
    ///
    /// dataflow-rs makes termination structural by requiring an author-supplied
    /// `max`, but does not bound what that number may be. A sweep can call a
    /// connector, so `max: 10_000_000` is a workflow that holds a request open
    /// until the channel timeout kills it, having spent the interim consuming
    /// pool connections — the same class of foot-gun `max_channel_call_depth`
    /// exists to prevent, which is why the bound is spelled the same way.
    /// Raise it when a workload genuinely needs more; `0` removes the ceiling
    /// and leaves only the author's `max`.
    pub max_loop_iterations: i64,
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
    /// Refuse to start when an enabled connector cannot be loaded — a missing
    /// `env://DB_PASSWORD`, an unparseable config, an unresolvable secret
    /// reference (F16).
    ///
    /// Default `false` keeps the historical behaviour: the connector is
    /// skipped with a log line and every workflow using it fails at request
    /// time instead. Set `true` in production so a bad rollout fails at boot,
    /// where the orchestrator will catch it, rather than hours later in
    /// request traffic. Only affects startup — a reload never takes the
    /// process down.
    pub fail_on_connector_load_error: bool,
    /// Ceiling on the operations one JSONLogic evaluation may perform, on
    /// every engine this node builds. `0` (the default) installs no ceiling.
    ///
    /// One operation is one dispatched node, one item an iterator examines,
    /// or what an operator charges for the data it moves — the tensor family
    /// charges per element. Constant-folded subtrees cost nothing. The
    /// ceiling is per *evaluation*, not per task or message: a task that
    /// evaluates ten expressions gets it ten times. It exists to bound
    /// expressions an author does not control — a model adapter written by a
    /// competitor, a rule a tenant uploads — deterministically, rather than
    /// with a wall-clock timeout that depends on the host.
    ///
    /// How a refusal surfaces is not uniform, and that is upstream's design.
    /// A custom function's template field (`http_call.path`, `crypto.data`,
    /// a model adapter) fails the task with `BUDGET_EXCEEDED`, non-retryable
    /// — `engine::error` keeps that variant through the handler's own error
    /// path. A built-in `map` mapping fails the task with status `500` and
    /// keeps the reason for the log, which is how the engine reports every
    /// mapping failure. A **condition** — workflow, task, group or `filter` —
    /// fails closed to `false` and is only logged, because condition
    /// evaluation has no error channel; a ceiling low enough to trip an
    /// ordinary condition therefore reads as "no workflow matched". Size it
    /// from the heaviest legitimate expression in the estate, not from the
    /// smallest.
    pub ops_budget: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            circuit_breaker: Default::default(),
            health_check_timeout_secs: 2,
            max_channel_call_depth: 10,
            default_channel_call_timeout_ms: 30_000,
            max_loop_iterations: 10_000,
            global_http_timeout_secs: 30,
            max_pool_cache_entries: 100,
            cache_cleanup_interval_secs: 60,
            max_memory_cache_entries: 100_000,
            rollout_sticky_header: String::new(),
            fail_on_connector_load_error: false,
            ops_budget: 0,
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
        Ok(())
    }
}
