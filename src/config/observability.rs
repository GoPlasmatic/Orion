use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TracingConfig {
    /// Enable OpenTelemetry trace export. Requires the `otel` feature flag at compile time.
    pub enabled: bool,
    /// OTLP gRPC endpoint (e.g. Jaeger, Grafana Tempo, OTel Collector).
    pub otlp_endpoint: String,
    /// Service name reported in traces.
    pub service_name: String,
    /// Sampling rate from 0.0 (none) to 1.0 (all).
    pub sample_rate: f64,
    /// Persistence policy for engine traces (rows written to the `traces` table).
    /// Unrelated to OpenTelemetry export above — this controls Orion's own
    /// per-request trace records that admins inspect via `/api/v1/data/traces`.
    pub storage: TracingStorageConfig,
    /// Allow per-request workflow profiling. When `true`, requests carrying
    /// `X-Orion-Profile: 1` (or `?profile=1`) receive a `profile` object in
    /// the response that breaks the request down by phase (engine lock,
    /// per-handler durations, trace store, residual workflow logic).
    ///
    /// Default `false` — the header is ignored in production until this is
    /// switched on, so attackers cannot probe internal timing.
    pub debug_profile_enabled: bool,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "orion".to_string(),
            sample_rate: 1.0,
            storage: TracingStorageConfig::default(),
            debug_profile_enabled: false,
        }
    }
}

/// Persistence mode for engine traces.
///
/// `Sync` writes inside the request path (default — strongest durability,
/// throughput capped by single-writer DB contention). `Async` enqueues to a
/// bounded background queue. `Batch` is the throughput-optimised path:
/// background workers accumulate writes and commit in one transaction.
/// `Off` disables persistence entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TraceStorageMode {
    /// Backwards-compatible default — existing deployments behave exactly as before.
    #[default]
    Sync,
    Async,
    Batch,
    Off,
}

/// Policy for the persistence queue when full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AsyncOnOverflow {
    /// Drop the trace; increment `trace_dropped_total{reason="overflow"}`.
    #[default]
    Drop,
    /// Wait up to `overflow_block_timeout_ms` for capacity, then drop.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TracingStorageConfig {
    /// Persistence policy. Applies to `store_completed` (sync result write)
    /// and `set_result` / `update_status` (async result writes). The async
    /// endpoint's `create_pending` step is always synchronous so the
    /// `GET /traces/{id}` contract is preserved after a 202.
    pub mode: TraceStorageMode,

    // ---- Filters (compose with mode; applied per trace) ----
    /// Fraction of traces to persist, 0.0–1.0. Roll a coin per trace; on a
    /// failed roll the trace is treated as `Off` and recorded in
    /// `trace_dropped_total{reason="sampled_out"}`.
    pub sample_rate: f64,

    /// When true, only persist traces that ended with errors
    /// (`message.has_errors()` for sync, `error_message.is_some()` for async).
    /// Successful traces are dropped with `reason="errors_only"`.
    pub errors_only: bool,

    // ---- Async / batch queue knobs ----
    /// Bounded mpsc capacity for the persistence queue.
    pub max_pending: usize,

    /// Behaviour when the queue is full (`async` and `batch` modes only).
    pub async_on_overflow: AsyncOnOverflow,

    /// When `async_on_overflow = "block"`, the producer waits at most this
    /// many milliseconds for capacity before dropping the trace.
    pub overflow_block_timeout_ms: u64,

    // ---- Async-mode-specific ----
    /// Worker count for `async` mode (one DB write per worker iteration).
    pub async_workers: usize,

    // ---- Batch-mode-specific ----
    /// Maximum entries accumulated before forcing a batch flush.
    pub batch_size: usize,

    /// Maximum time to wait before flushing a non-full batch (milliseconds).
    pub batch_flush_interval_ms: u64,

    /// Worker count for `batch` mode (each worker owns an independent batch).
    pub batch_workers: usize,
}

impl Default for TracingStorageConfig {
    fn default() -> Self {
        Self {
            mode: TraceStorageMode::Sync,
            sample_rate: 1.0,
            errors_only: false,
            max_pending: 10_000,
            async_on_overflow: AsyncOnOverflow::Drop,
            overflow_block_timeout_ms: 100,
            async_workers: 4,
            batch_size: 100,
            batch_flush_interval_ms: 100,
            batch_workers: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Allowed origins. Use `["*"]` (default) for permissive CORS.
    pub allowed_origins: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_string()],
        }
    }
}
