pub(crate) mod audit;
#[cfg(feature = "db-sqlite")]
pub(crate) mod backups;
pub(crate) mod channels;
pub(crate) mod connectors;
pub(crate) mod engine;
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
#[cfg(feature = "db-sqlite")]
pub(crate) use backups::{create_backup, list_backups};
pub(crate) use channels::{
    change_channel_status, create_channel, create_new_channel_version, delete_channel, get_channel,
    list_channel_versions, list_channels, update_channel,
};
pub(crate) use connectors::{
    create_connector, delete_connector, get_connector, list_circuit_breakers, list_connectors,
    reset_circuit_breaker, update_connector,
};
pub(crate) use engine::{engine_reload, engine_status};
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

pub fn admin_routes() -> Router<AppState> {
    let channel_routes = Router::new()
        .route("/", get(list_channels).post(create_channel))
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

    let mut router = Router::new()
        .nest("/channels", channel_routes)
        .nest("/workflows", workflow_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/audit-logs", audit_routes);

    #[cfg(feature = "db-sqlite")]
    {
        let backup_routes = Router::new().route("/", post(create_backup).get(list_backups));
        router = router.nest("/backups", backup_routes);
    }

    router
}
