pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod cache;
pub(crate) mod circuit_breakers;
pub(crate) mod cron;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod trace_dlq;

use axum::Extension;
use axum::Router;
use axum::routing::{get, post};
use serde_json::json;

use crate::admin_auth::AdminPrincipal;
use crate::runtime::state::AppState;

/// The audit actor when no admin credential was presented — which is every
/// request when `admin_auth.enabled = false`. `pub(crate)` because the v2
/// deployment store records the same principal *in its rows*, not only on the
/// audit trail, and the two spellings must not drift.
pub(crate) const ANONYMOUS_PRINCIPAL: &str = "anonymous";

/// The request context recorded alongside every audit row.
///
/// An audit trail whose only fields are *who* and *what* cannot answer the
/// question an investigation actually asks — *from where, and as part of which
/// request*. `client_ip` comes from the same trusted-proxy policy the rate
/// limiter uses, so a caller cannot dictate it with a forged
/// `X-Forwarded-For`; `request_id` ties the row to the access log and to the
/// `error.request_id` the client was handed.
///
/// `None` when the task-local is out of scope (a unit test calling a handler
/// directly), and individual fields are omitted when empty rather than
/// recorded as `""`.
fn request_details() -> Option<String> {
    let ctx = orion_common::request_context::current()?;
    let mut details = serde_json::Map::new();
    if !ctx.request_id.is_empty() {
        details.insert("request_id".into(), json!(ctx.request_id));
    }
    if !ctx.client_ip.is_empty() {
        details.insert("client_ip".into(), json!(ctx.client_ip));
    }
    if let Some(ua) = ctx.user_agent {
        details.insert("user_agent".into(), json!(ua));
    }
    // K5: what this mutation was part of, per the caller's own labelling
    // (`X-Orion-Change-Context`), so the trail groups a multi-request
    // operation without guesswork.
    if let Some(cc) = ctx.change_context {
        details.insert("change_context".into(), json!(cc));
    }
    (!details.is_empty()).then(|| serde_json::Value::Object(details).to_string())
}

/// Emit a structured audit log event for admin mutations.
///
/// The row goes onto the bounded, shutdown-drained
/// [`crate::queue::audit_queue`] rather than into a detached `tokio::spawn`,
/// so a mutation accepted moments before SIGTERM is still recorded and a slow
/// database cannot spawn one writer task per admin request.
pub(crate) fn audit_log(
    queue: &crate::queue::audit_queue::AuditQueue,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    queue.submit(audit_event(principal, action, resource_type, resource_id));
}

/// Build one audit event, and emit the parts of the trail that are not the
/// database row: the `audit`-target log line and the Prometheus counter.
fn audit_event(
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> crate::queue::audit_queue::AuditEvent {
    let event = audit_record(principal, action, resource_type, resource_id);
    emit_audit_trail(&event);
    event
}

/// One audit event and nothing else: the row a mutation that owns its
/// transaction writes inside it (the v2 deployment head move). Its log line
/// and counter wait for the commit — [`emit_audit_trail`] — so a mutation
/// that rolls back leaves no trace claiming it happened.
pub(crate) fn audit_record(
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> crate::queue::audit_queue::AuditEvent {
    let who = principal
        .as_ref()
        .map(|e| e.0.key_id.as_str())
        .unwrap_or(ANONYMOUS_PRINCIPAL);
    crate::queue::audit_queue::AuditEvent {
        principal: who.to_string(),
        action: action.to_string(),
        resource_type: resource_type.to_string(),
        resource_id: resource_id.to_string(),
        details: request_details(),
    }
}

/// The parts of the audit trail that are not the database row: the
/// `audit`-target log line and the Prometheus counter.
pub(crate) fn emit_audit_trail(event: &crate::queue::audit_queue::AuditEvent) {
    tracing::info!(
        target: "audit",
        principal = %event.principal,
        action = %event.action,
        resource_type = %event.resource_type,
        resource_id = %event.resource_id,
        details = event.details.as_deref().unwrap_or("{}"),
        "admin_audit_event"
    );
    crate::metrics::record_admin_audit(&event.action, &event.resource_type);
}

/// The admin API, with its own body limit.
///
/// `DefaultBodyLimit::max(ingest.max_payload_size)` — a name that says
/// *data plane* — was a single global layer, so admin bodies shared a ceiling
/// with anonymous channel traffic. Applied here it sits closer to the handler
/// than the global one, so it wins for these routes and nowhere else.
pub fn admin_routes(max_body_size: usize) -> Router<AppState> {
    // A cron channel's manual run: the id is the deployed channel's.
    let channel_routes = Router::new().route("/{id}/trigger", post(cron::trigger_channel));

    let connector_routes = Router::new()
        .route(
            "/circuit-breakers",
            get(circuit_breakers::list_circuit_breakers),
        )
        .route(
            "/circuit-breakers/{key}",
            post(circuit_breakers::reset_circuit_breaker),
        );

    let engine_routes = Router::new().route("/status", get(engine::engine_status));

    let cron_routes = Router::new()
        .route("/status", get(cron::cron_status))
        .route("/occurrences", get(cron::list_occurrences))
        .route("/occurrences/{id}", get(cron::get_occurrence))
        .route("/occurrences/{id}/retry", post(cron::retry_occurrence))
        .route("/occurrences/{id}/cancel", post(cron::cancel_occurrence));

    let cache_routes = Router::new().route(
        "/namespaces/{namespace}/invalidate",
        post(cache::invalidate_namespace),
    );

    let audit_routes = Router::new().route("/", get(audit::list_audit_logs));

    let function_routes = Router::new().route("/", get(functions::list_functions));

    // R8: the trace reads live on the admin plane because that is what they
    // are. `GET /traces` is admin-only, and `GET /traces/{id}` authenticates
    // itself (admin credential, or the per-submission capability token from
    // the async 202) — see `admin_auth::is_guarded_path`, which exempts it
    // from the blanket admin guard for exactly that reason.
    let trace_routes = Router::new()
        .route("/", get(crate::routes::data::traces::list_traces))
        .route("/{id}", get(crate::routes::data::traces::get_trace));

    let trace_dlq_routes = Router::new()
        .route("/", get(trace_dlq::list_trace_dlq))
        .route("/purge", post(trace_dlq::purge_trace_dlq))
        .route("/{id}", get(trace_dlq::get_trace_dlq_entry))
        .route("/{id}/requeue", post(trace_dlq::requeue_trace_dlq_entry));

    let backup_routes =
        Router::new().route("/", post(backups::create_backup).get(backups::list_backups));

    Router::new()
        .nest("/channels", channel_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/functions", function_routes)
        .nest("/cron", cron_routes)
        .nest("/cache", cache_routes)
        .nest("/audit-logs", audit_routes)
        .nest("/traces", trace_routes)
        .nest("/trace-dlq", trace_dlq_routes)
        .nest("/backups", backup_routes)
        .layer(axum::extract::DefaultBodyLimit::max(max_body_size))
}
