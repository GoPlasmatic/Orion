use axum::extract::State;
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::openapi::DataEnvelope;
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
        (status = 200, description = "Engine status: the generation this node serves, what it \
            could not load (`load_issues` — the same lists `/health` shows an admin), and what \
            this node is configured to run (`capabilities`). Describes the node that answered; \
            peers reload on their own and may differ.", body = DataEnvelope<orion_api::EngineStatusResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn engine_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let generation = state.runtime.load();
    let workflows = generation.engine.workflows();

    let mut channels: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut active_count = 0u64;

    for w in workflows.iter() {
        channels.insert(&w.channel);
        if matches!(w.status, dataflow_rs::WorkflowStatus::Active) {
            active_count += 1;
        }
    }

    let uptime = chrono::Utc::now() - state.start_time;
    let load_issues =
        crate::runtime::load_issues::collect(&generation, &state.connector_registry).await;

    Ok(data_response(orion_api::EngineStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime.num_seconds(),
        workflows_count: workflows.len() as u64,
        active_workflows: active_count,
        channels: channels.into_iter().map(str::to_string).collect(),
        generation: generation.id,
        load_issues: Some(load_issues),
        capabilities: Some(orion_api::EngineCapabilities {
            cron: state.config.cron.enabled,
            plugins: state.plugins.is_some(),
            models: state.models.is_some(),
        }),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/engine/reload",
    tag = "Engine",
    responses(
        (status = 200, description = "Engine reloaded. `generation` is the id of the generation \
            this reload published and `load_issues` is what *that* generation could not load — \
            an entity quarantined by it is not serving, though the reload succeeded.",
            body = DataEnvelope<orion_api::EngineReloadedResponse>),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn engine_reload(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    // `Now` always reloads, so the generation is always there; the fallback
    // is only the type's other arm.
    let generation = audit_and_reload(
        &state,
        &principal,
        "reload",
        "engine",
        "manual",
        super::ReloadMode::Now,
    )
    .await?
    .unwrap_or_else(|| state.runtime.load());
    let load_issues =
        crate::runtime::load_issues::collect(&generation, &state.connector_registry).await;

    Ok(data_response(orion_api::EngineReloadedResponse {
        reloaded: true,
        workflows_count: generation.engine.workflows().len() as u64,
        generation: generation.id,
        load_issues: Some(load_issues),
    }))
}
