use std::sync::atomic::{AtomicBool, Ordering};

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Global enable flag for metric recording. When false, every `record_*` helper
/// short-circuits before touching the `metrics` crate — this avoids ~2 % of
/// per-request CPU spent hashing labels and walking the recorder's indexmap
/// even when no real recorder is installed.
static METRICS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable metric recording globally. Call once at startup based on
/// `config.metrics.enabled`. Safe to call again later (e.g., from tests).
fn set_enabled(enabled: bool) {
    METRICS_ENABLED.store(enabled, Ordering::Relaxed);
}

#[inline(always)]
fn is_enabled() -> bool {
    METRICS_ENABLED.load(Ordering::Relaxed)
}

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
///
/// Must be called once at startup before any metrics are recorded.
/// Falls back to a local recorder handle if the global recorder is already installed.
pub fn init_metrics() -> PrometheusHandle {
    set_enabled(true);
    PrometheusBuilder::new()
        .install_recorder()
        .unwrap_or_else(|_| {
            // Recorder already installed (e.g., parallel tests) — create a standalone handle
            PrometheusBuilder::new().build_recorder().handle()
        })
}

// ---------------------------------------------------------------------------
// Counter helpers
// ---------------------------------------------------------------------------

/// Increment the messages_total counter.
pub fn record_message(channel: &str, status: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("messages_total", "channel" => channel.to_owned(), "status" => status).increment(1);
}

/// Increment the errors_total counter.
pub fn record_error(error_type: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("errors_total", "type" => error_type).increment(1);
}

/// Record a rejected admin-API authentication attempt.
///
/// Separate from `errors_total{type="auth_failure"}`, which it replaces for
/// this purpose: that counter is shared with ~15 unrelated `record_error` call
/// sites (`panic`, `dedup_backend`, `kafka_retry`, …), so alerting on
/// credential guessing meant a filter that also matched all of them
/// (proposal O11).
///
/// `reason` is one of `missing_or_malformed`, `invalid_key`, `locked_out`.
pub fn record_admin_auth_failure(reason: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("admin_auth_failures_total", "reason" => reason).increment(1);
}

// ---------------------------------------------------------------------------
// Histogram helpers
// ---------------------------------------------------------------------------

/// Record message processing duration.
pub fn record_message_duration(channel: &str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("message_duration_seconds", "channel" => channel.to_owned()).record(duration_secs);
}

// ---------------------------------------------------------------------------
// Gauge helpers
// ---------------------------------------------------------------------------

/// Record a circuit breaker trip event.
pub fn record_circuit_breaker_trip(connector: &str, channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "circuit_breaker_trips_total",
        "connector" => connector.to_owned(),
        "channel" => channel.to_owned()
    )
    .increment(1);
}

/// Record a request rejected by an open circuit breaker.
pub fn record_circuit_breaker_rejection(connector: &str, channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "circuit_breaker_rejections_total",
        "connector" => connector.to_owned(),
        "channel" => channel.to_owned()
    )
    .increment(1);
}

/// Set the active_workflows gauge.
pub fn set_active_workflows(count: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("active_workflows").set(count);
}

// ---------------------------------------------------------------------------
// HTTP & observability helpers
// ---------------------------------------------------------------------------

/// Record HTTP request count and duration in a single call.
///
/// Accepts owned `String` labels so callers can pass values they already
/// allocated without a redundant re-allocation.
pub fn record_http_request(method: String, path: String, status: u16, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    let status = status.to_string();
    counter!(
        "http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status.clone()
    )
    .increment(1);
    histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "path" => path,
        "status" => status
    )
    .record(duration_secs);
}

/// Record DB query duration.
fn record_db_query_duration(operation: &'static str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("db_query_duration_seconds", "operation" => operation).record(duration_secs);
}

/// Wrap an async operation with DB query timing.
pub async fn timed_db_op<F, T>(operation: &'static str, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let start = std::time::Instant::now();
    let result = f.await;
    record_db_query_duration(operation, start.elapsed().as_secs_f64());
    result
}

/// Record engine lock acquisition wait time.
pub fn record_engine_lock_wait(mode: &'static str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("engine_lock_wait_seconds", "mode" => mode).record(duration_secs);
}

/// Record engine reload duration.
pub fn record_engine_reload_duration(duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("engine_reload_duration_seconds").record(duration_secs);
}

/// Record engine reload event.
pub fn record_engine_reload(status: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("engine_reloads_total", "status" => status).increment(1);
}

/// Record a channel execution.
pub fn record_channel_execution(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("channel_executions_total", "channel" => channel.to_owned()).increment(1);
}

/// Record a rate-limit rejection. `scope` must come from a bounded set — a
/// registry-confirmed channel name or a route-group label, never
/// client-controlled input like the client IP, which spoofed
/// `X-Forwarded-For` values would turn into unbounded label cardinality (O1).
pub fn record_rate_limit_rejected(scope: &str) {
    if !is_enabled() {
        return;
    }
    counter!("rate_limit_rejections_total", "scope" => scope.to_owned()).increment(1);
}

/// Record a response cache hit.
pub fn record_cache_hit(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("response_cache_hits_total", "channel" => channel.to_owned()).increment(1);
}

/// Record a response cache miss.
pub fn record_cache_miss(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("response_cache_misses_total", "channel" => channel.to_owned()).increment(1);
}

// ---------------------------------------------------------------------------
// Trace queue gauges
// ---------------------------------------------------------------------------

/// Set the trace queue pending depth gauge.
pub fn set_trace_queue_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_queue_depth").set(depth);
}

/// Set the number of active trace worker tasks.
pub fn set_trace_workers_active(count: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_workers_active").set(count);
}

/// Set the total (max) trace worker capacity.
pub fn set_trace_workers_total(count: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_workers_total").set(count);
}

/// Set the approximate memory usage of queued trace payloads.
pub fn set_trace_queue_memory_bytes(bytes: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_queue_memory_bytes").set(bytes);
}

/// Count a submission the queue refused. `reason` is `"full"` (the bounded
/// buffer is at capacity) or `"memory"` (`max_queue_memory_bytes` exceeded).
/// Both surface to the caller as 503, so without this counter shedding is
/// indistinguishable from any other upstream error (O2).
pub fn record_trace_queue_rejected(reason: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("trace_queue_rejected_total", "reason" => reason).increment(1);
}

// ---------------------------------------------------------------------------
// Trace DLQ metrics
// ---------------------------------------------------------------------------

/// Set the number of rows in the trace DLQ. Refreshed by the DLQ retry loop
/// on every poll tick, so it stops updating if `queue.dlq_retry_enabled` is
/// false — which is itself the condition that makes the DLQ grow.
pub fn set_trace_dlq_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_dlq_depth").set(depth);
}

/// Count a DLQ entry reaching a terminal state for this cycle. `outcome` is
/// `"retried"` (resubmitted for another attempt), `"exhausted"` (gave up), or
/// `"failed"` (the retry attempt itself could not be made).
pub fn record_trace_dlq_retry(outcome: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("trace_dlq_retries_total", "outcome" => outcome).increment(1);
}

// ---------------------------------------------------------------------------
// Trace persistence queue metrics
// ---------------------------------------------------------------------------

/// Increment the dropped-trace counter. `reason` is one of:
/// `"overflow"`, `"sampled_out"`, `"errors_only"`, `"off"`.
pub fn record_trace_dropped(reason: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("trace_dropped_total", "reason" => reason).increment(1);
}

/// Set the persistence queue depth.
pub fn set_trace_persistence_queue_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("trace_persistence_queue_depth").set(depth);
}

/// Record a batch flush size (number of rows committed in one batch).
pub fn record_trace_persistence_batch_size(size: usize) {
    if !is_enabled() {
        return;
    }
    histogram!("trace_persistence_batch_size").record(size as f64);
}

/// Count a trace-storage write the persistence workers could not complete.
/// These writes are dropped after the failure (Q6 covers retrying them), so
/// this counter is the only signal that traces are being lost (O2).
pub fn record_trace_persistence_failure() {
    if !is_enabled() {
        return;
    }
    counter!("trace_persistence_failures_total").increment(1);
}

// ---------------------------------------------------------------------------
// Connector request metrics
// ---------------------------------------------------------------------------

/// Record a connector request outcome.
pub fn record_connector_request(connector: &str, channel: &str, status: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "connector_requests_total",
        "connector" => connector.to_owned(),
        "channel" => channel.to_owned(),
        "status" => status
    )
    .increment(1);
}

/// Record connector request duration.
pub fn record_connector_duration(connector: &str, channel: &str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!(
        "connector_request_duration_seconds",
        "connector" => connector.to_owned(),
        "channel" => channel.to_owned()
    )
    .record(duration_secs);
}

// ---------------------------------------------------------------------------
// Kafka consumer lag gauge
// ---------------------------------------------------------------------------

/// Set the consumer lag for a specific topic-partition.
pub fn set_kafka_consumer_lag(topic: &str, partition: i32, lag: f64) {
    if !is_enabled() {
        return;
    }
    gauge!(
        "kafka_consumer_lag",
        "topic" => topic.to_owned(),
        "partition" => partition.to_string()
    )
    .set(lag);
}

// ---------------------------------------------------------------------------
// Database pool gauges
// ---------------------------------------------------------------------------

/// Set the database connection pool size (total connections).
pub fn set_db_pool_size(size: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("db_pool_size").set(size);
}

/// Set the number of idle database connections.
pub fn set_db_pool_idle(idle: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("db_pool_idle").set(idle);
}

/// Record an admin audit event.
pub fn record_admin_audit(action: &str, resource_type: &str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "admin_audit_events_total",
        "action" => action.to_owned(),
        "resource_type" => resource_type.to_owned()
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_recorder() {
        let _ = PrometheusBuilder::new().install_recorder();
        // Tests exercise the recording path directly; opt in to the runtime gate.
        set_enabled(true);
    }

    #[test]
    fn test_record_message() {
        ensure_recorder();
        // Should not panic
        record_message("test-channel", "ok");
        record_message("test-channel", "error");
    }

    #[test]
    fn test_record_error() {
        ensure_recorder();
        record_error("engine");
        record_error("storage");
    }

    #[test]
    fn test_record_message_duration() {
        ensure_recorder();
        record_message_duration("orders", 0.123);
    }

    #[test]
    fn test_record_circuit_breaker_trip() {
        ensure_recorder();
        record_circuit_breaker_trip("my-connector", "orders");
    }

    #[test]
    fn test_record_circuit_breaker_rejection() {
        ensure_recorder();
        record_circuit_breaker_rejection("my-connector", "orders");
    }

    #[test]
    fn test_set_active_workflows() {
        ensure_recorder();
        set_active_workflows(5.0);
        set_active_workflows(0.0);
    }

    #[test]
    fn test_record_http_request() {
        ensure_recorder();
        record_http_request("GET".into(), "/health".into(), 200, 0.005);
        record_http_request("POST".into(), "/api/v1/data/orders".into(), 201, 0.010);
    }

    #[test]
    fn test_record_db_query_duration() {
        ensure_recorder();
        record_db_query_duration("list_rules", 0.010);
    }

    #[tokio::test]
    async fn test_timed_db_op() {
        ensure_recorder();
        let result = timed_db_op("test_op", async { 42 }).await;
        assert_eq!(result, 42);
    }

    #[test]
    fn test_record_engine_lock_wait() {
        ensure_recorder();
        record_engine_lock_wait("read", 0.001);
        record_engine_lock_wait("write", 0.050);
    }

    #[test]
    fn test_record_engine_reload_duration() {
        ensure_recorder();
        record_engine_reload_duration(0.250);
    }

    #[test]
    fn test_record_engine_reload() {
        ensure_recorder();
        record_engine_reload("success");
        record_engine_reload("failure");
    }

    #[test]
    fn test_record_channel_execution() {
        ensure_recorder();
        record_channel_execution("orders");
    }

    #[test]
    fn test_record_rate_limit_rejected() {
        ensure_recorder();
        record_rate_limit_rejected("orders");
        record_rate_limit_rejected("admin");
    }

    /// Render into a *local* recorder so the assertions see only what this
    /// test emitted — the global recorder is shared with every other test in
    /// the binary.
    fn render_local(f: impl FnOnce()) -> String {
        set_enabled(true);
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        ::metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    #[test]
    fn test_record_trace_queue_rejected() {
        let out = render_local(|| {
            record_trace_queue_rejected("full");
            record_trace_queue_rejected("full");
            record_trace_queue_rejected("memory");
        });
        assert!(
            out.contains(r#"trace_queue_rejected_total{reason="full"} 2"#),
            "missing full-queue rejections in:\n{out}"
        );
        assert!(
            out.contains(r#"trace_queue_rejected_total{reason="memory"} 1"#),
            "missing memory rejections in:\n{out}"
        );
    }

    #[test]
    fn test_record_trace_dlq_retry() {
        let out = render_local(|| {
            record_trace_dlq_retry("retried");
            record_trace_dlq_retry("exhausted");
            record_trace_dlq_retry("failed");
            record_trace_dlq_retry("exhausted");
        });
        assert!(
            out.contains(r#"trace_dlq_retries_total{outcome="retried"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"trace_dlq_retries_total{outcome="exhausted"} 2"#),
            "{out}"
        );
        assert!(
            out.contains(r#"trace_dlq_retries_total{outcome="failed"} 1"#),
            "{out}"
        );
    }

    #[test]
    fn test_set_trace_dlq_depth() {
        let out = render_local(|| {
            set_trace_dlq_depth(7.0);
            set_trace_dlq_depth(4.0);
        });
        assert!(
            out.contains("trace_dlq_depth 4"),
            "gauge must hold the latest value:\n{out}"
        );
    }

    #[test]
    fn test_record_trace_persistence_failure() {
        let out = render_local(|| {
            record_trace_persistence_failure();
            record_trace_persistence_failure();
            record_trace_persistence_failure();
        });
        assert!(out.contains("trace_persistence_failures_total 3"), "{out}");
    }

    #[test]
    fn test_init_metrics() {
        // Should return a handle even if already installed
        let handle = init_metrics();
        let output = handle.render();
        assert!(output.is_ascii());
    }
}
