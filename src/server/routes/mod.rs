pub mod admin;
pub mod data;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::server::state::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_endpoint))
        .nest("/api/v1/admin", admin::admin_routes())
        .nest("/api/v1/data", data::data_routes())
}

#[tracing::instrument(skip(state))]
async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let uptime = chrono::Utc::now() - state.start_time;

    // Check database connectivity
    let db_healthy = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db_pool)
        .await
        .is_ok();

    // Check engine state — independently verify the engine lock is acquirable
    let mut rules_loaded = 0;
    let engine_healthy =
        match tokio::time::timeout(std::time::Duration::from_secs(2), state.engine.read()).await {
            Ok(guard) => {
                rules_loaded = guard.workflows().len();
                true
            }
            Err(_) => false,
        };

    let overall_healthy = db_healthy && engine_healthy;
    let status_str = if overall_healthy { "ok" } else { "degraded" };
    let http_status = if overall_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = json!({
        "status": status_str,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "rules_loaded": rules_loaded,
        "components": {
            "database": if db_healthy { "ok" } else { "error" },
            "engine": if engine_healthy { "ok" } else { "error" },
        }
    });

    (http_status, Json(body))
}

async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let metrics = state.metrics_handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        metrics,
    )
}

/// Reload the engine with all active rules from the database.
#[tracing::instrument(skip(state))]
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

    let mut engine_write = state.engine.write().await;
    let new_engine = engine_write.with_new_workflows(workflows);
    *engine_write = Arc::new(new_engine);

    // Update active rules gauge
    crate::metrics::set_active_rules(rules.len() as f64);

    tracing::info!(rules_count = rules.len(), "Engine reloaded");
    Ok(())
}
