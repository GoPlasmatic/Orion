use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// Engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub circuit_breaker: CircuitBreakerConfig,
    /// Timeout in seconds for the health and readiness database ping.
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
    /// The filesystem root for SQLite providers and `db` connections. Relative
    /// paths resolve beneath it; absolute paths must stay inside it. Traversal,
    /// symlink escapes and reserved Orion state database names are refused.
    pub sqlite_connector_root: String,
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
    /// or what an operator charges for the data it moves. Constant-folded
    /// subtrees cost nothing. The ceiling is per *evaluation*, not per task
    /// or message: a task that evaluates ten expressions gets it ten times.
    /// It exists to bound expressions an author does not control — a rule a
    /// tenant uploads — deterministically, rather than with a wall-clock
    /// timeout that depends on the host.
    ///
    /// How a refusal surfaces is not uniform, and that is upstream's design.
    /// A custom function's template field (`http_call.path`, `crypto.data`)
    /// fails the task with `BUDGET_EXCEEDED`, non-retryable — `engine::error`
    /// keeps that variant through the handler's own error path. A built-in
    /// `map` mapping fails the task with status `500` and keeps the reason
    /// for the log, which is how the engine reports every
    /// mapping failure. A **condition** — workflow, task, group or `filter` —
    /// fails closed to `false` and is only logged, because condition
    /// evaluation has no error channel; a ceiling low enough to trip an
    /// ordinary condition therefore reads as "no workflow matched". Size it
    /// from the heaviest legitimate expression in the estate, not from the
    /// smallest.
    pub ops_budget: u64,
    /// Cumulative task-dispatch ceiling for one root run (including loops,
    /// fan-out branches, and nested `channel_call` invocations) (`TD-03`).
    /// `0` (default) disables the ceiling.
    pub max_run_tasks: u64,
    /// Cumulative expression-operation ceiling across one root run (`TD-03`).
    /// `0` (default) disables the ceiling.
    pub max_run_expr_ops: u64,
    /// Cumulative custom host-unit ceiling across one root run (`TD-03`).
    /// `0` (default) disables the ceiling.
    pub max_run_custom_units: u64,
    /// Maximum concurrent in-flight task dispatches within one root run (`TD-03`).
    /// `0` (default) disables the ceiling.
    pub max_run_in_flight: u64,
    /// Blocking native jobs that may run at once on this node, each on a
    /// thread of its own: a plugin component compile, and any job an
    /// extension schedules on the node's native work scheduler. Beyond it a
    /// job waits for a slot.
    pub native_work_max_in_flight: usize,
    /// How long a caller waits for a native job, the wait for a slot
    /// included. A caller past it fails; the job keeps its slot until its
    /// thread finishes, so the bound counts the threads actually busy.
    pub native_work_timeout_ms: u64,
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
            sqlite_connector_root: "./data/connectors/sqlite".to_string(),
            cache_cleanup_interval_secs: 60,
            max_memory_cache_entries: 100_000,
            rollout_sticky_header: String::new(),
            fail_on_connector_load_error: false,
            ops_budget: 0,
            max_run_tasks: 0,
            max_run_expr_ops: 0,
            max_run_custom_units: 0,
            max_run_in_flight: 0,
            native_work_max_in_flight: 4,
            native_work_timeout_ms: 300_000,
        }
    }
}

impl EngineConfig {
    /// Convert configured `0`-or-positive run budget fields into [`RunBudgetLimits`].
    pub fn run_budget_limits(&self) -> RunBudgetLimits {
        RunBudgetLimits {
            max_tasks: (self.max_run_tasks > 0).then_some(self.max_run_tasks),
            max_expr_ops: (self.max_run_expr_ops > 0).then_some(self.max_run_expr_ops),
            max_custom_units: (self.max_run_custom_units > 0).then_some(self.max_run_custom_units),
            max_in_flight: (self.max_run_in_flight > 0).then_some(self.max_run_in_flight),
        }
    }

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
        require_nonzero(
            self.native_work_max_in_flight as u64,
            "engine.native_work_max_in_flight",
        )?;
        require_nonzero(self.native_work_timeout_ms, "engine.native_work_timeout_ms")?;
        Ok(())
    }
}

/// Configuration for circuit breakers. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub failure_threshold: u32,
    pub recovery_timeout_secs: u64,
    /// Maximum number of tracked circuit breakers. Oldest entries are evicted when exceeded.
    pub max_breakers: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_threshold: 5,
            recovery_timeout_secs: 30,
            max_breakers: 10_000,
        }
    }
}

/// Optional run-wide resource ceilings enforced by `orion_server::engine::budget::SharedRunAccountant`.
///
/// A field set to `None` is tracked without a hard ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunBudgetLimits {
    /// Maximum cumulative task dispatches across the root run and any nested
    /// child runs sharing this accountant.
    pub max_tasks: Option<u64>,
    /// Maximum cumulative expression operations evaluated across the run.
    pub max_expr_ops: Option<u64>,
    /// Maximum cumulative custom host units charged via `RunAccountant::charge_custom`.
    pub max_custom_units: Option<u64>,
    /// Maximum concurrent in-flight task dispatches (`OccupancyGuard`s alive
    /// simultaneously).
    pub max_in_flight: Option<u64>,
}

impl RunBudgetLimits {
    /// Whether no ceiling is set. An accountant with these limits can never
    /// refuse anything, so a run need not carry one at all.
    pub fn is_unbounded(&self) -> bool {
        *self == Self::default()
    }
}
