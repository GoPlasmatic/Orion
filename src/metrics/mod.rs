use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
///
/// Must be called once at startup before any metrics are recorded.
/// Falls back to a local recorder handle if the global recorder is already installed.
pub fn init_metrics() -> PrometheusHandle {
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
pub fn record_message(channel: &str, status: &str) {
    counter!("messages_total", "channel" => channel.to_string(), "status" => status.to_string())
        .increment(1);
}

/// Increment the errors_total counter.
pub fn record_error(error_type: &str) {
    counter!("errors_total", "type" => error_type.to_string()).increment(1);
}

// ---------------------------------------------------------------------------
// Histogram helpers
// ---------------------------------------------------------------------------

/// Record message processing duration.
pub fn record_message_duration(channel: &str, duration_secs: f64) {
    histogram!("message_duration_seconds", "channel" => channel.to_string()).record(duration_secs);
}

// ---------------------------------------------------------------------------
// Gauge helpers
// ---------------------------------------------------------------------------

/// Record a circuit breaker trip event.
pub fn record_circuit_breaker_trip(connector: &str, channel: &str) {
    counter!(
        "circuit_breaker_trips_total",
        "connector" => connector.to_string(),
        "channel" => channel.to_string()
    )
    .increment(1);
}

/// Record a request rejected by an open circuit breaker.
pub fn record_circuit_breaker_rejection(connector: &str, channel: &str) {
    counter!(
        "circuit_breaker_rejections_total",
        "connector" => connector.to_string(),
        "channel" => channel.to_string()
    )
    .increment(1);
}

/// Set the active_workflows gauge.
pub fn set_active_workflows(count: f64) {
    gauge!("active_workflows").set(count);
}

// ---------------------------------------------------------------------------
// HTTP & observability helpers
// ---------------------------------------------------------------------------

/// Record an HTTP request metric.
pub fn record_http_request(method: &str, path: &str, status: u16) {
    counter!(
        "http_requests_total",
        "method" => method.to_string(),
        "path" => path.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// Record HTTP request duration.
pub fn record_http_request_duration(method: &str, path: &str, status: u16, duration_secs: f64) {
    histogram!(
        "http_request_duration_seconds",
        "method" => method.to_string(),
        "path" => path.to_string(),
        "status" => status.to_string()
    )
    .record(duration_secs);
}

/// Record DB query duration.
pub fn record_db_query_duration(operation: &str, duration_secs: f64) {
    histogram!("db_query_duration_seconds", "operation" => operation.to_string())
        .record(duration_secs);
}

/// Wrap an async operation with DB query timing.
pub async fn timed_db_op<F, T>(operation: &str, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let start = std::time::Instant::now();
    let result = f.await;
    record_db_query_duration(operation, start.elapsed().as_secs_f64());
    result
}

/// Record engine lock acquisition wait time.
pub fn record_engine_lock_wait(mode: &str, duration_secs: f64) {
    histogram!("engine_lock_wait_seconds", "mode" => mode.to_string()).record(duration_secs);
}

/// Record engine reload duration.
pub fn record_engine_reload_duration(duration_secs: f64) {
    histogram!("engine_reload_duration_seconds").record(duration_secs);
}

/// Record engine reload event.
pub fn record_engine_reload(status: &str) {
    counter!("engine_reloads_total", "status" => status.to_string()).increment(1);
}

/// Record a channel execution.
pub fn record_channel_execution(channel: &str) {
    counter!("channel_executions_total", "channel" => channel.to_string()).increment(1);
}

/// Record a rate-limit rejection.
pub fn record_rate_limit_rejected(client: &str) {
    counter!("rate_limit_rejections_total", "client" => client.to_string()).increment(1);
}

/// Record a response cache hit.
pub fn record_cache_hit(channel: &str) {
    counter!("response_cache_hits_total", "channel" => channel.to_string()).increment(1);
}

/// Record a response cache miss.
pub fn record_cache_miss(channel: &str) {
    counter!("response_cache_misses_total", "channel" => channel.to_string()).increment(1);
}

// ---------------------------------------------------------------------------
// Trace queue gauges
// ---------------------------------------------------------------------------

/// Set the trace queue pending depth gauge.
pub fn set_trace_queue_depth(depth: f64) {
    gauge!("trace_queue_depth").set(depth);
}

/// Set the number of active trace worker tasks.
pub fn set_trace_workers_active(count: f64) {
    gauge!("trace_workers_active").set(count);
}

/// Set the total (max) trace worker capacity.
pub fn set_trace_workers_total(count: f64) {
    gauge!("trace_workers_total").set(count);
}

/// Set the approximate memory usage of queued trace payloads.
pub fn set_trace_queue_memory_bytes(bytes: f64) {
    gauge!("trace_queue_memory_bytes").set(bytes);
}

// ---------------------------------------------------------------------------
// Connector request metrics
// ---------------------------------------------------------------------------

/// Record a connector request outcome.
pub fn record_connector_request(connector: &str, channel: &str, status: &str) {
    counter!(
        "connector_requests_total",
        "connector" => connector.to_string(),
        "channel" => channel.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// Record connector request duration.
pub fn record_connector_duration(connector: &str, channel: &str, duration_secs: f64) {
    histogram!(
        "connector_request_duration_seconds",
        "connector" => connector.to_string(),
        "channel" => channel.to_string()
    )
    .record(duration_secs);
}

// ---------------------------------------------------------------------------
// Kafka consumer lag gauge
// ---------------------------------------------------------------------------

/// Set the consumer lag for a specific topic-partition.
#[cfg(feature = "kafka")]
pub fn set_kafka_consumer_lag(topic: &str, partition: i32, lag: f64) {
    gauge!(
        "kafka_consumer_lag",
        "topic" => topic.to_string(),
        "partition" => partition.to_string()
    )
    .set(lag);
}

// ---------------------------------------------------------------------------
// Database pool gauges
// ---------------------------------------------------------------------------

/// Set the database connection pool size (total connections).
pub fn set_db_pool_size(size: f64) {
    gauge!("db_pool_size").set(size);
}

/// Set the number of idle database connections.
pub fn set_db_pool_idle(idle: f64) {
    gauge!("db_pool_idle").set(idle);
}

/// Record an admin audit event.
pub fn record_admin_audit(action: &str, resource_type: &str) {
    counter!(
        "admin_audit_events_total",
        "action" => action.to_string(),
        "resource_type" => resource_type.to_string()
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_recorder() {
        let _ = PrometheusBuilder::new().install_recorder();
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
        record_http_request("GET", "/health", 200);
        record_http_request("POST", "/api/v1/data/orders", 201);
    }

    #[test]
    fn test_record_http_request_duration() {
        ensure_recorder();
        record_http_request_duration("GET", "/health", 200, 0.005);
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
        record_rate_limit_rejected("192.168.1.1");
    }

    #[test]
    fn test_init_metrics() {
        // Should return a handle even if already installed
        let handle = init_metrics();
        let output = handle.render();
        assert!(output.is_ascii());
    }
}
