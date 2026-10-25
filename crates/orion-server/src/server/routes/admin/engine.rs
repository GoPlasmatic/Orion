use axum::Json;
use axum::extract::State;
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::routes::openapi::DataEnvelope;
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

// ============================================================
// Engine Control
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v2/admin/engine/status",
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
    let workflows = generation.workflows();

    let mut channels: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut active_count = 0u64;

    for w in workflows.iter() {
        channels.insert(&w.channel);
        if w.status == dataflow_rs::def::Status::Active {
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
        }),
    }))
}
