use axum::extract::State;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::openapi::{DataEnvelope, EngineReloaded, EngineStatus};
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

use super::audit_and_reload;

// ============================================================
// Engine Control
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/engine/status",
    tag = "Engine",
    responses(
        (status = 200, description = "Engine status", body = DataEnvelope<EngineStatus>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn engine_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let engine = state.engine.load();
    let workflows = engine.workflows();

    let mut channels: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut active_count = 0u64;

    for w in workflows.iter() {
        channels.insert(&w.channel);
        if matches!(w.status, dataflow_rs::WorkflowStatus::Active) {
            active_count += 1;
        }
    }

    let uptime = chrono::Utc::now() - state.start_time;

    Ok(data_response(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "workflows_count": workflows.len(),
        "active_workflows": active_count,
        "channels": channels.into_iter().collect::<Vec<_>>(),
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/engine/reload",
    tag = "Engine",
    responses(
        (status = 200, description = "Engine reloaded", body = DataEnvelope<EngineReloaded>),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn engine_reload(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    audit_and_reload(&state, &principal, "reload", "engine", "manual").await?;

    let engine = state.engine.load();
    let workflows_count = engine.workflows().len();

    Ok(data_response(json!({
        "reloaded": true,
        "workflows_count": workflows_count,
    })))
}
