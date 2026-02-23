use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::server::state::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health_check))
        .nest("/api/v1", v1_routes())
}

fn v1_routes() -> Router<AppState> {
    Router::new()
        .route("/rules", get(list_rules).post(create_rule))
        .route(
            "/rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/connectors", get(list_connectors).post(create_connector))
        .route(
            "/connectors/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        .route("/process", axum::routing::post(process_message))
        .route(
            "/process/{channel}",
            axum::routing::post(process_message_for_channel),
        )
}

// -- Health --

async fn health_check(State(state): State<AppState>) -> Json<Value> {
    let engine = state.engine.read().await;
    let workflows = engine.workflows();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "rules_loaded": workflows.len(),
    }))
}

// -- Rules (stub handlers for Phase 1) --

async fn list_rules(
    State(state): State<AppState>,
    axum::extract::Query(filter): axum::extract::Query<
        crate::storage::repositories::rules::RuleFilter,
    >,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let rules = state.rule_repo.list(&filter).await?;
    Ok(Json(json!({ "data": rules })))
}

async fn create_rule(
    State(state): State<AppState>,
    Json(req): Json<crate::storage::repositories::rules::CreateRuleRequest>,
) -> Result<(axum::http::StatusCode, Json<Value>), crate::errors::OrionError> {
    let rule = state.rule_repo.create(&req).await?;
    reload_engine(&state).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "data": rule })),
    ))
}

async fn get_rule(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let rule = state.rule_repo.get_by_id(&id).await?;
    Ok(Json(json!({ "data": rule })))
}

async fn update_rule(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<crate::storage::repositories::rules::UpdateRuleRequest>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let rule = state.rule_repo.update(&id, &req).await?;
    reload_engine(&state).await?;
    Ok(Json(json!({ "data": rule })))
}

async fn delete_rule(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::http::StatusCode, crate::errors::OrionError> {
    state.rule_repo.delete(&id).await?;
    reload_engine(&state).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// -- Connectors (stub handlers for Phase 1) --

async fn list_connectors(
    State(state): State<AppState>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let connectors = state.connector_repo.list().await?;
    Ok(Json(json!({ "data": connectors })))
}

async fn create_connector(
    State(state): State<AppState>,
    Json(req): Json<crate::storage::repositories::connectors::CreateConnectorRequest>,
) -> Result<(axum::http::StatusCode, Json<Value>), crate::errors::OrionError> {
    let connector = state.connector_repo.create(&req).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "data": connector })),
    ))
}

async fn get_connector(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let connector = state.connector_repo.get_by_id(&id).await?;
    Ok(Json(json!({ "data": connector })))
}

async fn update_connector(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<crate::storage::repositories::connectors::UpdateConnectorRequest>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let connector = state.connector_repo.update(&id, &req).await?;
    Ok(Json(json!({ "data": connector })))
}

async fn delete_connector(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::http::StatusCode, crate::errors::OrionError> {
    state.connector_repo.delete(&id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// -- Data Processing (stub handlers for Phase 1) --

async fn process_message(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let engine = state.engine.read().await;
    let mut message = dataflow_rs::Message::from_value(&payload);
    engine
        .process_message(&mut message)
        .await
        .map_err(crate::errors::OrionError::Engine)?;
    Ok(Json(json!({
        "id": message.id,
        "data": message.data(),
        "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
    })))
}

async fn process_message_for_channel(
    State(state): State<AppState>,
    axum::extract::Path(channel): axum::extract::Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, crate::errors::OrionError> {
    let engine = state.engine.read().await;
    let mut message = dataflow_rs::Message::from_value(&payload);
    engine
        .process_message_for_channel(&channel, &mut message)
        .await
        .map_err(crate::errors::OrionError::Engine)?;
    Ok(Json(json!({
        "id": message.id,
        "data": message.data(),
        "errors": message.errors.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
    })))
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
