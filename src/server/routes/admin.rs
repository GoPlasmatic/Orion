use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::routes::reload_engine;
use crate::server::state::AppState;
use crate::storage::repositories::connectors::{CreateConnectorRequest, UpdateConnectorRequest};
use crate::storage::repositories::rules::{
    CreateRuleRequest, RuleFilter, StatusChangeRequest, UpdateRuleRequest,
};

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        // Rules
        .route("/rules", get(list_rules).post(create_rule))
        .route("/rules/import", post(import_rules))
        .route("/rules/export", get(export_rules))
        .route(
            "/rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/rules/{id}/status", patch(change_rule_status))
        .route("/rules/{id}/test", post(test_rule))
        // Connectors
        .route("/connectors", get(list_connectors).post(create_connector))
        .route(
            "/connectors/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        // Engine
        .route("/engine/status", get(engine_status))
        .route("/engine/reload", post(engine_reload))
}

// ============================================================
// Rules CRUD
// ============================================================

#[tracing::instrument(skip(state))]
async fn list_rules(
    State(state): State<AppState>,
    Query(filter): Query<RuleFilter>,
) -> Result<Json<Value>, OrionError> {
    let rules = state.rule_repo.list(&filter).await?;
    Ok(Json(json!({ "data": rules })))
}

#[tracing::instrument(skip(state, req))]
async fn create_rule(
    State(state): State<AppState>,
    Json(req): Json<CreateRuleRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let rule = state.rule_repo.create(&req).await?;
    reload_engine(&state).await?;
    Ok((StatusCode::CREATED, Json(json!({ "data": rule }))))
}

#[tracing::instrument(skip(state))]
async fn get_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let rule = state.rule_repo.get_by_id(&id).await?;
    let version_count = state.rule_repo.count_versions(&id).await?;
    Ok(Json(json!({
        "data": rule,
        "version_count": version_count,
    })))
}

#[tracing::instrument(skip(state, req))]
async fn update_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRuleRequest>,
) -> Result<Json<Value>, OrionError> {
    let rule = state.rule_repo.update(&id, &req).await?;
    reload_engine(&state).await?;
    Ok(Json(json!({ "data": rule })))
}

#[tracing::instrument(skip(state))]
async fn delete_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.rule_repo.delete(&id).await?;
    reload_engine(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Rule Status Management
// ============================================================

#[tracing::instrument(skip(state, req))]
async fn change_rule_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let rule = state.rule_repo.update_status(&id, &req.status).await?;
    reload_engine(&state).await?;
    Ok(Json(json!({ "data": rule })))
}

// ============================================================
// Rule Dry-Run / Testing
// ============================================================

#[derive(Deserialize)]
struct TestRuleRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
}

#[tracing::instrument(skip(state, req))]
async fn test_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TestRuleRequest>,
) -> Result<Json<Value>, OrionError> {
    use crate::storage::repositories::rules::rule_to_workflow;

    let rule = state.rule_repo.get_by_id(&id).await?;
    let workflow = rule_to_workflow(&rule)?;

    // Create an isolated engine with just this one rule, including custom functions
    let custom_fns = crate::engine::build_custom_functions(state.connector_registry.clone());
    let test_engine = dataflow_rs::Engine::new(vec![workflow], Some(custom_fns));

    let mut payload = json!({});
    if let Some(obj) = req.data.as_object() {
        for (k, v) in obj {
            payload[k] = v.clone();
        }
    } else {
        payload = req.data;
    }

    let mut message = dataflow_rs::Message::from_value(&payload);
    super::data::merge_metadata(&mut message, &req.metadata);

    let trace = test_engine
        .process_message_with_trace(&mut message)
        .await
        .map_err(OrionError::Engine)?;

    let matched = !trace.steps.is_empty()
        && trace.steps.iter().any(|s| {
            matches!(
                s.result,
                dataflow_rs::StepResult::Executed | dataflow_rs::StepResult::Skipped
            )
        });

    Ok(Json(json!({
        "matched": matched,
        "trace": serde_json::to_value(&trace).unwrap_or_default(),
        "output": message.data(),
        "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    })))
}

// ============================================================
// Rule Import / Export
// ============================================================

#[tracing::instrument(skip(state, rules), fields(count = rules.len()))]
async fn import_rules(
    State(state): State<AppState>,
    Json(rules): Json<Vec<CreateRuleRequest>>,
) -> Result<Json<Value>, OrionError> {
    let results = state.rule_repo.bulk_create(&rules).await?;

    let mut imported = 0u64;
    let mut failed = 0u64;
    let mut errors = Vec::new();

    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(_) => imported += 1,
            Err(e) => {
                failed += 1;
                errors.push(json!({
                    "index": i,
                    "error": e.to_string(),
                }));
            }
        }
    }

    if imported > 0 {
        reload_engine(&state).await?;
    }

    Ok(Json(json!({
        "imported": imported,
        "failed": failed,
        "errors": errors,
    })))
}

#[tracing::instrument(skip(state))]
async fn export_rules(
    State(state): State<AppState>,
    Query(filter): Query<RuleFilter>,
) -> Result<Json<Value>, OrionError> {
    let rules = state.rule_repo.list(&filter).await?;
    Ok(Json(json!({ "data": rules })))
}

// ============================================================
// Connectors CRUD
// ============================================================

/// Reload the connector registry after a mutation.
async fn reload_connectors(state: &AppState) -> Result<(), OrionError> {
    state
        .connector_registry
        .reload(state.connector_repo.as_ref())
        .await?;
    Ok(())
}

#[tracing::instrument(skip(state))]
async fn list_connectors(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    let connectors = state.connector_repo.list().await?;
    let masked: Vec<_> = connectors.iter().map(mask_connector).collect();
    Ok(Json(json!({ "data": masked })))
}

#[tracing::instrument(skip(state, req))]
async fn create_connector(
    State(state): State<AppState>,
    Json(req): Json<CreateConnectorRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let connector = state.connector_repo.create(&req).await?;
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok((StatusCode::CREATED, Json(json!({ "data": masked }))))
}

#[tracing::instrument(skip(state))]
async fn get_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let connector = state.connector_repo.get_by_id(&id).await?;
    let masked = mask_connector(&connector);
    Ok(Json(json!({ "data": masked })))
}

#[tracing::instrument(skip(state, req))]
async fn update_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConnectorRequest>,
) -> Result<Json<Value>, OrionError> {
    let connector = state.connector_repo.update(&id, &req).await?;
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok(Json(json!({ "data": masked })))
}

#[tracing::instrument(skip(state))]
async fn delete_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.connector_repo.delete(&id).await?;
    reload_connectors(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Engine Control
// ============================================================

#[tracing::instrument(skip(state))]
async fn engine_status(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    let engine = state.engine.read().await;
    let workflows = engine.workflows();

    let mut channels: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut active_count = 0u64;
    let mut paused_count = 0u64;

    for w in workflows.iter() {
        channels.insert(&w.channel);
        match w.status {
            dataflow_rs::WorkflowStatus::Active => active_count += 1,
            dataflow_rs::WorkflowStatus::Paused => paused_count += 1,
            _ => {}
        }
    }

    let uptime = chrono::Utc::now() - state.start_time;

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "rules_count": workflows.len(),
        "active_rules": active_count,
        "paused_rules": paused_count,
        "channels": channels.into_iter().collect::<Vec<_>>(),
    })))
}

#[tracing::instrument(skip(state))]
async fn engine_reload(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    reload_engine(&state).await?;

    let engine = state.engine.read().await;
    let rules_count = engine.workflows().len();

    Ok(Json(json!({
        "reloaded": true,
        "rules_count": rules_count,
    })))
}
