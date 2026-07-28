pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod trace_dlq;
pub(crate) mod workflows;

use axum::Router;
use axum::routing::{get, patch, post};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use axum::Extension;

use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::reload_engine;
use crate::server::state::AppState;
use crate::storage::repositories::audit_logs::AuditLogRepository;

// Re-export all handler functions so that `super::admin::list_channels` etc. still works
// (needed by openapi.rs and integration tests).
pub(crate) use audit::list_audit_logs;
pub(crate) use backups::{create_backup, list_backups};
pub(crate) use channels::{
    change_channel_status, create_channel, create_new_channel_version, delete_channel, get_channel,
    import_channels, list_channel_versions, list_channels, update_channel,
};
pub(crate) use connectors::{
    create_connector, delete_connector, get_connector, import_connectors, list_circuit_breakers,
    list_connectors, reset_circuit_breaker, update_connector,
};
pub(crate) use engine::{engine_reload, engine_status};
pub(crate) use functions::list_functions;
pub(crate) use trace_dlq::{
    get_trace_dlq_entry, list_trace_dlq, purge_trace_dlq, requeue_trace_dlq_entry,
};
pub(crate) use workflows::{
    change_workflow_status, create_new_workflow_version, create_workflow, delete_workflow,
    export_workflows, get_workflow, import_workflows, list_workflow_versions, list_workflows,
    test_workflow, update_rollout, update_workflow, validate_workflow,
};

/// Largest batch any `/import` endpoint accepts.
///
/// Each item is a separate in-request DB round-trip holding a connection, plus
/// an audit-log write, and the only previous bound was the global 1 MB body
/// limit — which is tens of thousands of minimal JSON objects. That is a
/// self-inflicted DoS on the admin plane (proposal R14). Larger migrations
/// should be chunked.
pub(crate) const MAX_IMPORT_ITEMS: usize = 1000;

/// Reject an oversized import batch before any work is done.
pub(crate) fn check_import_batch_size(len: usize) -> Result<(), crate::errors::OrionError> {
    if len > MAX_IMPORT_ITEMS {
        return Err(crate::errors::OrionError::validation(format!(
            "import accepts at most {MAX_IMPORT_ITEMS} items per request, got {len} — \
             split the batch"
        )));
    }
    Ok(())
}

/// Fold per-item outcomes into the shared bulk-import counters:
/// `(succeeded, failed, [{index, error}])`.
///
/// Per-item errors go through `OrionError::client_message`, not `to_string`:
/// these strings are embedded in a **200** body, so they bypass the redaction
/// `IntoResponse` would otherwise apply and would leak raw sqlx driver text
/// (proposal G5).
pub(crate) fn fold_import_results(
    results: impl IntoIterator<Item = Result<(), crate::errors::OrionError>>,
) -> (u64, u64, Vec<serde_json::Value>) {
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut errors = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(()) => ok += 1,
            Err(e) => {
                failed += 1;
                errors.push(json!({"index": i, "error": e.client_message()}));
            }
        }
    }
    (ok, failed, errors)
}

/// Per-item driver for the `Vec<Value>` import endpoints: deserialize each
/// item (so a single shape/enum typo becomes one failed entry instead of
/// aborting the whole batch), validate it, and — unless `dry_run` — create it.
/// Dry-run is pure validation: no DB reads, so name conflicts are not
/// detected there (they surface as Conflict on the real import).
pub(crate) async fn import_items<T, V, C, Fut>(
    items: Vec<serde_json::Value>,
    dry_run: bool,
    validate: V,
    create: C,
) -> (u64, u64, Vec<serde_json::Value>)
where
    T: serde::de::DeserializeOwned,
    V: Fn(&T) -> Result<(), crate::errors::OrionError>,
    C: Fn(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::errors::OrionError>>,
{
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut errors = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let mut fail = |e: String| {
            failed += 1;
            errors.push(json!({"index": i, "error": e}));
        };
        let parsed: T = match serde_json::from_value(item) {
            Ok(v) => v,
            Err(e) => {
                fail(e.to_string());
                continue;
            }
        };
        if let Err(e) = validate(&parsed) {
            fail(e.client_message());
            continue;
        }
        if dry_run {
            ok += 1;
        } else if let Err(e) = create(parsed).await {
            fail(e.client_message());
        } else {
            ok += 1;
        }
    }
    (ok, failed, errors)
}

/// The `?dry_run=true` response envelope shared by all three import endpoints.
pub(crate) fn dry_run_response(
    would_create: u64,
    would_fail: u64,
    errors: Vec<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "dry_run": true,
        "would_create": would_create,
        "would_fail": would_fail,
        "imported": 0,
        "failed": would_fail,
        "errors": errors,
    }))
}

/// The real-import response envelope shared by all three import endpoints.
pub(crate) fn import_response(
    imported: u64,
    failed: u64,
    errors: Vec<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "imported": imported,
        "failed": failed,
        "errors": errors,
    }))
}

/// Shared version filter used by both channel and workflow version endpoints.
#[derive(Debug, Deserialize)]
pub(crate) struct VersionFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Emit a structured audit log event for admin mutations.
/// Persists to the database via fire-and-forget to avoid blocking the response.
fn audit_log(
    repo: &Arc<dyn AuditLogRepository>,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    let who = principal
        .as_ref()
        .map(|e| e.0.key_prefix.as_str())
        .unwrap_or("anonymous");
    tracing::info!(
        target: "audit",
        principal = %who,
        action = %action,
        resource_type = %resource_type,
        resource_id = %resource_id,
        "admin_audit_event"
    );
    crate::metrics::record_admin_audit(action, resource_type);

    // Read the request-scoped id here: `tokio::spawn` below starts a fresh
    // task that does not inherit task-locals.
    let details = crate::server::request_context::REQUEST_ID
        .try_with(|id| id.clone())
        .ok()
        .filter(|id| !id.is_empty())
        .map(|id| json!({ "request_id": id }).to_string());

    // Fire-and-forget DB persistence — audit logging must never block admin responses
    let repo = repo.clone();
    let who = who.to_string();
    let action = action.to_string();
    let resource_type = resource_type.to_string();
    let resource_id = resource_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = repo
            .insert(
                &who,
                &action,
                &resource_type,
                &resource_id,
                details.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "Failed to persist audit log entry");
        }
    });
}

/// Record an audit-log event for a mutation that intentionally does NOT
/// trigger an engine reload because the target is a draft (drafts are not
/// in the engine). Use at draft create/update/import call sites so the
/// no-reload choice is explicit at the call site rather than implied by
/// the absence of [`audit_and_reload`].
fn audit_log_draft_only(
    repo: &Arc<dyn AuditLogRepository>,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    audit_log(repo, principal, action, resource_type, resource_id);
}

/// Record an audit-log event and trigger an engine reload. The standard
/// post-mutation sequence for admin operations that change the active set
/// (activate / archive / delete / update-rollout). Drafts do NOT reload —
/// use [`audit_log_draft_only`] in those code paths.
async fn audit_and_reload(
    state: &AppState,
    principal: &Option<Extension<AdminPrincipal>>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) -> Result<(), crate::errors::OrionError> {
    audit_log(
        &state.audit_log_repo,
        principal,
        action,
        resource_type,
        resource_id,
    );
    reload_engine(state).await?;
    state.cluster.bump_config_epoch().await
}

pub fn admin_routes() -> Router<AppState> {
    let channel_routes = Router::new()
        .route("/", get(list_channels).post(create_channel))
        .route("/import", post(import_channels))
        .route(
            "/{id}",
            get(get_channel).put(update_channel).delete(delete_channel),
        )
        .route("/{id}/status", patch(change_channel_status))
        .route(
            "/{id}/versions",
            get(list_channel_versions).post(create_new_channel_version),
        );

    let workflow_routes = Router::new()
        .route("/", get(list_workflows).post(create_workflow))
        .route("/import", post(import_workflows))
        .route("/export", get(export_workflows))
        .route("/validate", post(validate_workflow))
        .route(
            "/{id}",
            get(get_workflow)
                .put(update_workflow)
                .delete(delete_workflow),
        )
        .route("/{id}/status", patch(change_workflow_status))
        .route(
            "/{id}/versions",
            get(list_workflow_versions).post(create_new_workflow_version),
        )
        .route("/{id}/rollout", patch(update_rollout))
        .route("/{id}/test", post(test_workflow));

    let connector_routes = Router::new()
        .route("/", get(list_connectors).post(create_connector))
        .route("/import", post(import_connectors))
        .route(
            "/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        .route("/circuit-breakers", get(list_circuit_breakers))
        .route("/circuit-breakers/{key}", post(reset_circuit_breaker));

    let engine_routes = Router::new()
        .route("/status", get(engine_status))
        .route("/reload", post(engine_reload));

    let audit_routes = Router::new().route("/", get(list_audit_logs));

    let function_routes = Router::new().route("/", get(list_functions));

    let trace_dlq_routes = Router::new()
        .route("/", get(list_trace_dlq))
        .route("/purge", post(purge_trace_dlq))
        .route("/{id}", get(get_trace_dlq_entry))
        .route("/{id}/requeue", post(requeue_trace_dlq_entry));

    let mut router = Router::new()
        .nest("/channels", channel_routes)
        .nest("/workflows", workflow_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/functions", function_routes)
        .nest("/audit-logs", audit_routes)
        .nest("/trace-dlq", trace_dlq_routes);

    let backup_routes = Router::new().route("/", post(create_backup).get(list_backups));
    router = router.nest("/backups", backup_routes);

    router
}
