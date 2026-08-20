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
pub(crate) fn set_enabled(enabled: bool) {
    METRICS_ENABLED.store(enabled, Ordering::Relaxed);
}

#[inline(always)]
pub(crate) fn is_enabled() -> bool {
    METRICS_ENABLED.load(Ordering::Relaxed)
}

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
///
/// Must be called once at startup before any metrics are recorded.
/// Falls back to a local recorder handle if the global recorder is already installed.
pub fn init_metrics() -> PrometheusHandle {
    init_metrics_with_instance(None)
}

/// Latency buckets, in seconds, spanning sub-millisecond in-process work
/// (engine lock waits) through multi-second external calls.
///
/// Setting buckets is not cosmetic: without them
/// `metrics-exporter-prometheus` renders every `histogram!` as a **summary
/// with pre-computed quantiles**, which cannot be aggregated across replicas.
/// That directly contradicts cluster mode, where the whole point is N
/// replicas behind one load balancer (proposal O13).
const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Buckets for batch-size histograms, which count rows rather than seconds.
const SIZE_BUCKETS: &[f64] = &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
///
/// Must be called once at startup before any metrics are recorded.
/// Falls back to a local recorder handle if the global recorder is already installed.
///
/// `instance_id` is stamped on every metric as an `instance` label so
/// per-replica attribution does not depend entirely on the scrape config.
pub fn init_metrics_with_instance(instance_id: Option<&str>) -> PrometheusHandle {
    set_enabled(true);
    let build = || {
        let mut b = PrometheusBuilder::new();
        // Suffix matching: one call covers every `*_seconds` family.
        b = b
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix("_seconds".to_string()),
                LATENCY_BUCKETS,
            )
            .expect("latency buckets are non-empty and finite");
        b = b
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix("_batch_size".to_string()),
                SIZE_BUCKETS,
            )
            .expect("size buckets are non-empty and finite");
        if let Some(id) = instance_id {
            b = b.add_global_label("instance", id);
        }
        b
    };
    build().install_recorder().unwrap_or_else(|_| {
        // Recorder already installed (e.g., parallel tests) — create a standalone handle
        build().build_recorder().handle()
    })
}

// ---------------------------------------------------------------------------
// Counter helpers
// ---------------------------------------------------------------------------

/// One refused JWT, by typed reason (#267) — the wire stays uniform, the
/// operator's dashboard does not have to.
pub fn record_jwt_rejection(reason: &'static str) {
    counter!("orion_jwt_rejections_total", "reason" => reason).increment(1);
}

/// One managed-OAuth2 token-endpoint request (#268). `outcome` is a bounded
/// set: `ok`, `rejected` (invalid_grant and friends — non-retryable),
/// `transport_error` (unreachable/5xx — retryable).
pub fn record_oauth_token_request(connector: &str, outcome: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "orion_oauth_token_requests_total",
        "connector" => connector.to_owned(),
        "outcome" => outcome
    )
    .increment(1);
}

/// Count one message through a channel, whatever its outcome.
///
/// O14: this is now the *only* per-channel invocation counter.
/// `orion_channel_executions_total{channel}` used to be incremented next to
/// the `status="ok"` arm of this one, so it was `orion_messages_total` minus
/// the label that says how the message ended — strictly less information
/// under a second name.
///
/// `sum by (channel) (orion_messages_total)` is the replacement, and it is a
/// **superset**, not an identity: the deleted counter had two call sites, both
/// on the HTTP path, and never saw the Kafka ingest or DLQ paths that also
/// record messages. Expect the per-channel rate to be higher than the old
/// series on any deployment that consumes from Kafka — that is the blind spot
/// closing, not double counting.
pub fn record_message(channel: &str, status: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_messages_total", "channel" => channel.to_owned(), "status" => status)
        .increment(1);
}

/// Increment the errors_total counter.
///
/// O14: the label is `reason`, not `type`. Every other categorical label in
/// this file is `reason`, `kind`, `outcome` or `status`; `type` was the lone
/// exception, and it is also a reserved-feeling word in PromQL tooling.
pub fn record_error(reason: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_errors_total", "reason" => reason).increment(1);
}

/// Publish the build-identity gauge, always `1`.
///
/// The standard way to answer "which version is each replica running?" from
/// Prometheus — the rollout and canary query. Previously `GIT_HASH` and
/// `BUILD_TIMESTAMP` surfaced only in `--version`, one boot log line, and the
/// admin-gated `/health` body, none of which a scrape can join against
/// (proposal O11).
pub fn record_build_info() {
    if !is_enabled() {
        return;
    }
    gauge!(
        "orion_build_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "git_hash" => env!("GIT_HASH"),
        "build_timestamp" => env!("BUILD_TIMESTAMP"),
    )
    .set(1.0);
}

/// Record a rejected admin-API authentication attempt.
///
/// Separate from `errors_total{reason="auth_failure"}`, which it replaces for
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
    counter!("orion_admin_auth_failures_total", "reason" => reason).increment(1);
}

// ---------------------------------------------------------------------------
// Histogram helpers
// ---------------------------------------------------------------------------

/// Record message processing duration.
pub fn record_message_duration(channel: &str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("orion_message_duration_seconds", "channel" => channel.to_owned())
        .record(duration_secs);
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
        "orion_circuit_breaker_trips_total",
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
        "orion_circuit_breaker_rejections_total",
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
    gauge!("orion_active_workflows").set(count);
}

// ---------------------------------------------------------------------------
// HTTP & observability helpers
// ---------------------------------------------------------------------------

/// Record HTTP request count and duration in a single call.
///
/// Borrowed labels, owned only past the `is_enabled` gate. They used to be
/// `String` parameters, on the reasoning that the caller had already allocated
/// them — but the caller allocated them *for this call*, so with metrics
/// disabled the request paid two allocations to reach an early return. The
/// labels are cardinality-bounded (`method` is a verb, `path` is the matched
/// route template), so they cannot be borrowed as `&'static str` and must be
/// owned to become metric labels; the question is only whether that happens
/// when nothing will read them.
pub fn record_http_request(method: &str, path: &str, status: u16, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    let status = status.to_string();
    counter!(
        "orion_http_requests_total",
        "method" => method.to_owned(),
        "path" => path.to_owned(),
        "status" => status.clone()
    )
    .increment(1);
    histogram!(
        "orion_http_request_duration_seconds",
        "method" => method.to_owned(),
        "path" => path.to_owned(),
        "status" => status
    )
    .record(duration_secs);
}

/// Record DB query duration.
fn record_db_query_duration(operation: &'static str, duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("orion_db_query_duration_seconds", "operation" => operation).record(duration_secs);
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

/// Record engine reload duration.
pub fn record_engine_reload_duration(duration_secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!("orion_engine_reload_duration_seconds").record(duration_secs);
}

/// Record engine reload event.
pub fn record_engine_reload(status: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_engine_reloads_total", "status" => status).increment(1);
}

/// Record a rate-limit rejection. `scope` must come from a bounded set — a
/// registry-confirmed channel name or a route-group label, never
/// client-controlled input like the client IP, which spoofed
/// `X-Forwarded-For` values would turn into unbounded label cardinality (O1).
pub fn record_rate_limit_rejected(scope: &str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_rate_limit_rejections_total", "scope" => scope.to_owned()).increment(1);
}

/// Record a rate-limit rejection caused by an *uncomputable key*, as distinct
/// from a caller being over its limit.
///
/// Split from [`record_rate_limit_rejected`] — which it accompanies rather than
/// replaces, so 429 accounting stays whole — because the two demand opposite
/// responses. Over-limit is the control working; a key that will not compute is
/// a misconfiguration that silently disables the control for every caller on
/// the channel, and it is invisible in an aggregate rejection count. A non-zero
/// rate here always means a channel needs its `key_logic` or `key_headers`
/// looked at.
///
/// `channel` is a registry-confirmed name, never client-controlled input (O1).
pub fn record_rate_limit_key_unavailable(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_rate_limit_key_unavailable_total", "channel" => channel.to_owned())
        .increment(1);
}

/// Record a response cache hit.
pub fn record_cache_hit(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_response_cache_hits_total", "channel" => channel.to_owned()).increment(1);
}

/// Record a response cache miss.
pub fn record_cache_miss(channel: &str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_response_cache_misses_total", "channel" => channel.to_owned()).increment(1);
}

// ---------------------------------------------------------------------------
// Background job health
// ---------------------------------------------------------------------------

/// Stamp the last-success gauge for a named background job with the current
/// unix time, in seconds.
///
/// The periodic jobs — trace cleanup, audit-log cleanup, DLQ retry, the
/// cluster epoch watcher, and the Kafka lag poller — deliberately swallow
/// per-tick errors and keep looping, because a transient DB blip must not
/// kill the loop. That left a *sustained* failure with no alertable signal:
/// the task was still running, but had not done its job in hours, and
/// cleanup/retry silently stopped cluster-wide (O3). Alert on
/// `time() - orion_job_last_success_timestamp_seconds{job="…"} > threshold`.
///
/// `job` is one of `trace_cleanup`, `audit_cleanup`, `dlq_retry`,
/// `epoch_watcher`, `kafka_lag`. Called once per fully successful tick;
/// a tick skipped because another node holds the job lease does not count —
/// on that node the gauge honestly goes stale, and the holder keeps it fresh.
pub fn record_job_success(job: &'static str) {
    if !is_enabled() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    gauge!("orion_job_last_success_timestamp_seconds", "job" => job).set(now);
}

// ---------------------------------------------------------------------------
// Trace queue gauges
// ---------------------------------------------------------------------------

/// Set the trace queue pending depth gauge.
pub fn set_trace_queue_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_trace_queue_depth").set(depth);
}

/// Set the number of active trace worker tasks.
pub fn set_trace_workers_active(count: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_trace_workers_active").set(count);
}

/// Set the total (max) trace worker capacity.
pub fn set_trace_workers_total(count: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_trace_workers_total").set(count);
}

/// Set the approximate memory usage of queued trace payloads.
pub fn set_trace_queue_memory_bytes(bytes: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_trace_queue_memory_bytes").set(bytes);
}

/// Count a submission the queue refused. `reason` is `"full"` (the bounded
/// buffer is at capacity) or `"memory"` (`max_queue_memory_bytes` exceeded).
/// Both surface to the caller as 503, so without this counter shedding is
/// indistinguishable from any other upstream error (O2).
pub fn record_trace_queue_rejected(reason: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_trace_queue_rejected_total", "reason" => reason).increment(1);
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
    gauge!("orion_trace_dlq_depth").set(depth);
}

/// Count a DLQ entry reaching a terminal state for this cycle. `outcome` is
/// `"retried"` (resubmitted for another attempt), `"exhausted"` (gave up), or
/// `"failed"` (the retry attempt itself could not be made).
pub fn record_trace_dlq_retry(outcome: &'static str) {
    if !is_enabled() {
        return;
    }
    counter!("orion_trace_dlq_retries_total", "outcome" => outcome).increment(1);
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
    counter!("orion_trace_dropped_total", "reason" => reason).increment(1);
}

/// Set the persistence queue depth.
pub fn set_trace_persistence_queue_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_trace_persistence_queue_depth").set(depth);
}

/// Record a batch flush size (number of rows committed in one batch).
pub fn record_trace_persistence_batch_size(size: usize) {
    if !is_enabled() {
        return;
    }
    histogram!("orion_trace_persistence_batch_size").record(size as f64);
}

/// Count a trace-storage write the persistence workers could not complete.
/// These writes are dropped after the failure (Q6 covers retrying them), so
/// this counter is the only signal that traces are being lost (O2).
pub fn record_trace_persistence_failure() {
    if !is_enabled() {
        return;
    }
    counter!("orion_trace_persistence_failures_total").increment(1);
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
        "orion_connector_requests_total",
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
        "orion_connector_request_duration_seconds",
        "connector" => connector.to_owned(),
        "channel" => channel.to_owned()
    )
    .record(duration_secs);
}

// ---------------------------------------------------------------------------
// Per-task engine timing
// ---------------------------------------------------------------------------

/// Record one workflow task's body duration.
///
/// Fed by `engine::observer`, which is the only way to time the eight sync
/// built-ins (`map`, `validate`, `filter`, `parse_*`, `publish_*`, `log`):
/// they are dispatched inside a private executor method and never reach the
/// handler registry, so no host can wrap them. Before this they were invisible
/// to Prometheus entirely and showed up only as the unattributed
/// `workflow_overhead_ms` residual in the opt-in profile surface.
///
/// `orion_connector_request_duration_seconds` remains the connector-scoped
/// view; this is keyed by *task*, so three `db_read` tasks in one workflow are
/// finally distinguishable.
///
/// Label cardinality is bounded by the deployed workflow set — `workflow` and
/// `task` are authored ids, not caller-supplied values, so no request can grow
/// the label space.
pub fn record_task_duration(workflow: &str, task: &str, function: &'static str, secs: f64) {
    if !is_enabled() {
        return;
    }
    histogram!(
        "orion_task_duration_seconds",
        "workflow" => workflow.to_owned(),
        "task" => task.to_owned(),
        "function" => function
    )
    .record(secs);
}

// ---------------------------------------------------------------------------
// Kafka consumer lag gauge
// ---------------------------------------------------------------------------

/// Set the consumer lag, in messages, for a specific topic-partition.
///
/// O14: this was the one family in the process that carried neither the
/// `orion_` prefix (so it could collide with any other exporter's
/// `kafka_consumer_lag` in a shared registry — the exact collision the prefix
/// convention exists to prevent) nor a unit suffix.
pub fn set_kafka_consumer_lag(topic: &str, partition: i32, lag: f64) {
    if !is_enabled() {
        return;
    }
    gauge!(
        "orion_kafka_consumer_lag_messages",
        "topic" => topic.to_owned(),
        "partition" => partition.to_string()
    )
    .set(lag);
}

/// `1` while Kafka ingestion is down — a consumer (re)start failed and the
/// supervisor has not recovered it yet — and `0` otherwise. Mirrors the
/// `kafka` component of `/readyz` so the outage is alertable from a scrape
/// as well as from the probe (K7).
pub fn set_kafka_ingest_degraded(degraded: bool) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_kafka_ingest_degraded").set(if degraded { 1.0 } else { 0.0 });
}

// ---------------------------------------------------------------------------
// Database pool gauges
// ---------------------------------------------------------------------------

/// Set the database connection pool size (total connections).
pub fn set_db_pool_size(size: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_db_pool_size").set(size);
}

/// Set the number of idle database connections.
pub fn set_db_pool_idle(idle: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_db_pool_idle").set(idle);
}

// ---------------------------------------------------------------------------
// Admin audit trail
// ---------------------------------------------------------------------------

/// Record an admin audit event.
pub fn record_admin_audit(action: &str, resource_type: &str) {
    if !is_enabled() {
        return;
    }
    counter!(
        "orion_admin_audit_events_total",
        "action" => action.to_owned(),
        "resource_type" => resource_type.to_owned()
    )
    .increment(1);
}

/// Count admin actions that happened but were **not** recorded (O7).
///
/// Any non-zero value means the audit trail has a hole in it, so this is the
/// counter to alert on outright rather than to threshold. `reason` is
/// `"queue_full"` (the writer fell `audit.max_pending` behind), `"write_failed"`
/// (the INSERT itself failed), `"drain_timeout"` (shutdown gave up on the
/// remaining rows) or `"writer_stopped"` (submitted after the writer exited).
pub fn record_audit_events_dropped(reason: &'static str, count: u64) {
    if !is_enabled() || count == 0 {
        return;
    }
    counter!("orion_audit_events_dropped_total", "reason" => reason).increment(count);
}

/// [`record_audit_events_dropped`] for the single-event case.
pub fn record_audit_event_dropped(reason: &'static str) {
    record_audit_events_dropped(reason, 1);
}

/// Set the number of audit events accepted but not yet written.
pub fn set_audit_queue_depth(depth: f64) {
    if !is_enabled() {
        return;
    }
    gauge!("orion_audit_queue_depth").set(depth);
}

/// Run `f` against a Prometheus recorder local to this thread and return the
/// exposition it produced, so assertions see only what `f` emitted — the
/// global recorder is shared with every other test in the binary.
///
/// Thread-local, not task-local: a `tokio` current-thread runtime driven
/// inside `f` (`rt.block_on(...)`) runs its spawned tasks on this same
/// thread, so a background writer's `record_*` calls land here too. That is
/// how `queue::audit_queue` asserts on its drop counter.
#[cfg(test)]
pub(crate) fn render_local(f: impl FnOnce()) -> String {
    set_enabled(true);
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    ::metrics::with_local_recorder(&recorder, f);
    handle.render()
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
        record_http_request("GET", "/health", 200, 0.005);
        record_http_request("POST", "/api/v1/data/orders", 201, 0.010);
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
    fn test_record_rate_limit_rejected() {
        ensure_recorder();
        record_rate_limit_rejected("orders");
        record_rate_limit_rejected("admin");
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

    /// O3: a successful job tick must stamp the gauge with a plausible unix
    /// time, labelled by job — the signal operators alert on going stale.
    #[test]
    fn test_record_job_success_stamps_unix_time_per_job() {
        let out = render_local(|| {
            record_job_success("trace_cleanup");
            record_job_success("dlq_retry");
        });
        let value: f64 = out
            .lines()
            .find(|l| {
                l.starts_with(r#"orion_job_last_success_timestamp_seconds{job="trace_cleanup"}"#)
            })
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or_default();
        // 2001-09-09 in unix seconds — anything above proves it's a real
        // timestamp, not a counter, a default zero, or a missing series.
        assert!(
            value > 1e9,
            "expected a unix-seconds gauge for trace_cleanup, got {value}; output:\n{out}"
        );
        assert!(
            out.contains(r#"orion_job_last_success_timestamp_seconds{job="dlq_retry"}"#),
            "each job must get its own series:\n{out}"
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

    // -- O14: metric and label naming ------------------------------------

    /// `orion_errors_total` is labelled `reason`, matching every other
    /// categorical label in this file. It used to be `type`.
    #[test]
    fn errors_total_is_labelled_by_reason() {
        let out = render_local(|| record_error("engine"));
        assert!(
            out.contains(r#"orion_errors_total{reason="engine"}"#),
            "errors must be labelled by reason:\n{out}"
        );
        assert!(
            !out.contains(r#"type="engine""#),
            "the old `type` label must be gone:\n{out}"
        );
    }

    /// One per-channel invocation counter, not two.
    /// `orion_channel_executions_total` was `orion_messages_total` minus the
    /// status label; recording a message must not resurrect it.
    #[test]
    fn one_per_channel_invocation_counter() {
        let out = render_local(|| {
            record_message("orders", "ok");
            record_message("orders", "error");
            record_message_duration("orders", 0.01);
        });
        assert!(
            out.contains(r#"orion_messages_total{channel="orders",status="ok"} 1"#),
            "messages must carry channel + status:\n{out}"
        );
        assert!(
            out.contains(r#"orion_messages_total{channel="orders",status="error"} 1"#),
            "the error arm must land on the same family:\n{out}"
        );
        assert!(
            !out.contains("channel_executions_total"),
            "the redundant second counter must stay gone:\n{out}"
        );
    }

    /// The Kafka lag gauge carries the `orion_` prefix (no collisions in a
    /// shared registry) and its unit. It used to be a bare
    /// `kafka_consumer_lag`.
    #[test]
    fn kafka_lag_gauge_is_prefixed_and_carries_its_unit() {
        let out = render_local(|| set_kafka_consumer_lag("orders", 3, 42.0));
        assert!(
            out.contains(r#"orion_kafka_consumer_lag_messages{"#),
            "the lag gauge must be prefixed and unit-suffixed:\n{out}"
        );
        assert!(out.contains(r#"topic="orders""#), "{out}");
        assert!(out.contains(r#"partition="3""#), "{out}");
        assert!(
            !out.contains("# TYPE kafka_consumer_lag gauge"),
            "the unprefixed family must stay gone:\n{out}"
        );
    }

    // -- O7: the audit trail's own metrics --------------------------------

    /// The counter the observability page tells operators to alert on
    /// outright. Each reason is a distinct series, and the batch form adds
    /// its count rather than one.
    #[test]
    fn test_record_audit_events_dropped() {
        let out = render_local(|| {
            record_audit_event_dropped("queue_full");
            record_audit_event_dropped("queue_full");
            record_audit_event_dropped("write_failed");
            record_audit_events_dropped("drain_timeout", 7);
            // A zero-length loss is not an event; it must not create a series
            // that an "alert on existence" rule would then fire on.
            record_audit_events_dropped("writer_stopped", 0);
        });
        assert!(
            out.contains(r#"orion_audit_events_dropped_total{reason="queue_full"} 2"#),
            "{out}"
        );
        assert!(
            out.contains(r#"orion_audit_events_dropped_total{reason="write_failed"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"orion_audit_events_dropped_total{reason="drain_timeout"} 7"#),
            "the batch form must add its count, not one:\n{out}"
        );
        assert!(
            !out.contains(r#"reason="writer_stopped""#),
            "a zero-count drop must not create a series:\n{out}"
        );
    }

    #[test]
    fn test_set_audit_queue_depth() {
        let out = render_local(|| {
            set_audit_queue_depth(12.0);
            set_audit_queue_depth(3.0);
        });
        assert!(
            out.contains("orion_audit_queue_depth 3"),
            "gauge must hold the latest value:\n{out}"
        );
    }

    #[test]
    fn test_init_metrics() {
        // Should return a handle even if already installed
        let handle = init_metrics();
        let output = handle.render();
        assert!(output.is_ascii());
    }
}
