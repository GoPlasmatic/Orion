pub(crate) mod audit;
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
pub(crate) mod functions;
pub(crate) mod workflows;

use axum::Router;
use axum::routing::{get, patch, post};
use serde::Deserialize;
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
pub(crate) use workflows::{
    change_workflow_status, create_new_workflow_version, create_workflow, delete_workflow,
    export_workflows, get_workflow, import_workflows, list_workflow_versions, list_workflows,
    test_workflow, update_rollout, update_workflow, validate_workflow,
};

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

    // Fire-and-forget DB persistence — audit logging must never block admin responses
    let repo = repo.clone();
    let who = who.to_string();
    let action = action.to_string();
    let resource_type = resource_type.to_string();
    let resource_id = resource_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = repo
            .insert(&who, &action, &resource_type, &resource_id, None)
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
    bump_config_epoch(state).await
}

/// Advance the cluster config epoch after a successful mutation + inline
/// reload, so other nodes' epoch watchers resync from the DB. Runs even with
/// cluster disabled (keeps the counter monotonic so enabling cluster later
/// starts sane) but only propagates failures when enabled — on a single
/// node a failed bump changes nothing, while in a cluster it means the
/// change did NOT propagate and the admin must retry.
pub(crate) async fn bump_config_epoch(
    state: &AppState,
) -> Result<(), crate::errors::OrionError> {
    match state.cluster.repo.bump_epoch().await {
        Ok(epoch) => {
            // fetch_max, not store: the inline reload already applied this
            // node's own change, but a concurrently observed higher epoch
            // must never be masked.
            state
                .cluster
                .last_seen_epoch
                .fetch_max(epoch, std::sync::atomic::Ordering::AcqRel);
            Ok(())
        }
        Err(e) if state.cluster.enabled => Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to bump config epoch (cluster disabled — ignored)");
            Ok(())
        }
    }
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

    let mut router = Router::new()
        .nest("/channels", channel_routes)
        .nest("/workflows", workflow_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/functions", function_routes)
        .nest("/audit-logs", audit_routes);

    let backup_routes = Router::new().route("/", post(create_backup).get(list_backups));
    router = router.nest("/backups", backup_routes);

    router
}
