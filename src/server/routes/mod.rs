pub mod admin;
pub mod data;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::server::state::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health_check))
        .nest("/api/v1/admin", admin::admin_routes())
        .nest("/api/v1/data", data::data_routes())
}

async fn health_check(State(state): State<AppState>) -> Json<Value> {
    let engine = state.engine.read().await;
    let workflows = engine.workflows();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "rules_loaded": workflows.len(),
    }))
}

/// Reload the engine with all active rules from the database.
pub async fn reload_engine(state: &AppState) -> Result<(), crate::errors::OrionError> {
    let rules = state.rule_repo.list_active().await?;
    let mut workflows = Vec::new();

    for rule in &rules {
        match crate::storage::repositories::rules::rule_to_workflow(rule) {
            Ok(w) => workflows.push(w),
            Err(e) => {
                tracing::warn!(rule_id = %rule.id, error = %e, "Failed to convert rule to workflow, skipping");
            }
        }
    }

    let engine_guard = state.engine.read().await;
    let new_engine = engine_guard.with_new_workflows(workflows);
    drop(engine_guard);

    let mut engine_write = state.engine.write().await;
    *engine_write = Arc::new(new_engine);

    tracing::info!(rules_count = rules.len(), "Engine reloaded");
    Ok(())
}
