pub mod admin;
pub mod data;
pub mod openapi;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::server::state::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_endpoint))
        .merge(SwaggerUi::new("/docs").url("/api/v1/openapi.json", openapi::ApiDoc::openapi()))
        .nest("/api/v1/admin", admin::admin_routes())
        .nest("/api/v1/data", data::data_routes())
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Operational",
    responses(
        (status = 200, description = "Service healthy"),
        (status = 503, description = "Service degraded"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let uptime = chrono::Utc::now() - state.start_time;

    // Check database connectivity
    let db_healthy = state.rule_repo.ping().await.is_ok();

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

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Operational",
    responses(
        (status = 200, description = "Prometheus metrics", content_type = "text/plain"),
    )
)]
pub(crate) async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
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
    let workflows = crate::engine::build_engine_workflows(&rules);

    // Build the new engine outside the write lock to minimize lock hold time.
    // Clone the current engine Arc, build new workflows, then swap atomically.
    let current_engine = state.engine.read().await.clone();
    let new_engine = Arc::new(current_engine.with_new_workflows(workflows));

    let mut engine_write =
        tokio::time::timeout(std::time::Duration::from_secs(10), state.engine.write())
            .await
            .map_err(|_| {
                crate::errors::OrionError::Internal(
                    "Engine reload timed out waiting for write lock".into(),
                )
            })?;
    *engine_write = new_engine;

    // Update active rules gauge
    crate::metrics::set_active_rules(rules.len() as f64);

    tracing::info!(rules_count = rules.len(), "Engine reloaded");
    Ok(())
}
