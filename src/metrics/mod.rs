use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Initialize the Prometheus metrics recorder and return a handle for rendering.
///
/// Must be called once at startup before any metrics are recorded.
pub fn init_metrics() -> PrometheusHandle {
    let builder = PrometheusBuilder::new();
    builder
        .install_recorder()
        .expect("failed to install Prometheus recorder")
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

/// Set the active_rules gauge.
pub fn set_active_rules(count: f64) {
    gauge!("active_rules").set(count);
}

