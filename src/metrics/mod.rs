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

/// Increment the rules_matched_total counter.
pub fn record_rule_matched(rule_id: &str, channel: &str) {
    counter!("rules_matched_total", "rule_id" => rule_id.to_string(), "channel" => channel.to_string())
        .increment(1);
}

/// Increment the task_executions_total counter.
pub fn record_task_execution(function: &str, status: &str) {
    counter!("task_executions_total", "function" => function.to_string(), "status" => status.to_string())
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

/// Record task execution duration.
pub fn record_task_duration(function: &str, duration_secs: f64) {
    histogram!("task_duration_seconds", "function" => function.to_string()).record(duration_secs);
}

// ---------------------------------------------------------------------------
// Gauge helpers
// ---------------------------------------------------------------------------

/// Set the active_rules gauge.
pub fn set_active_rules(count: f64) {
    gauge!("active_rules").set(count);
}

/// Set the pending_jobs gauge.
pub fn set_pending_jobs(count: f64) {
    gauge!("pending_jobs").set(count);
}
