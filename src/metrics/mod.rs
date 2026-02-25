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

/// Set the active_rules gauge.
pub fn set_active_rules(count: f64) {
    gauge!("active_rules").set(count);
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
