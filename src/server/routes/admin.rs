use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::routes::reload_engine;
use crate::server::state::AppState;
use crate::storage::models::RuleResponse;
use crate::storage::repositories::connectors::{
    ConnectorFilter, CreateConnectorRequest, UpdateConnectorRequest,
};
use crate::storage::repositories::rules::{
    CreateRuleRequest, RolloutUpdateRequest, RuleFilter, StatusChangeRequest, UpdateRuleRequest,
};
use crate::validation;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        // Rules
        .route("/rules", get(list_rules).post(create_rule))
        .route("/rules/import", post(import_rules))
        .route("/rules/export", get(export_rules))
        .route("/rules/validate", post(validate_rule))
        .route(
            "/rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/rules/{id}/status", patch(change_rule_status))
        .route(
            "/rules/{id}/versions",
            get(list_rule_versions).post(create_new_version),
        )
        .route("/rules/{id}/rollout", patch(update_rollout))
        .route("/rules/{id}/test", post(test_rule))
        // Connectors
        .route("/connectors", get(list_connectors).post(create_connector))
        .route(
            "/connectors/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        // Circuit breakers
        .route("/connectors/circuit-breakers", get(list_circuit_breakers))
        .route(
            "/connectors/circuit-breakers/{key}",
            post(reset_circuit_breaker),
        )
        // Engine
        .route("/engine/status", get(engine_status))
        .route("/engine/reload", post(engine_reload))
}

// ============================================================
// Rules CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/rules",
    params(RuleFilter),
    tag = "Rules",
    responses(
        (status = 200, description = "Paginated list of rules"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_rules(
    State(state): State<AppState>,
    Query(filter): Query<RuleFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.rule_repo.list_paginated(&filter).await?;
    let data: Vec<RuleResponse> = result
        .data
        .iter()
        .map(RuleResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "data": data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rules",
    tag = "Rules",
    request_body = CreateRuleRequest,
    responses(
        (status = 201, description = "Rule created as draft"),
        (status = 400, description = "Invalid input"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn create_rule(
    State(state): State<AppState>,
    Json(req): Json<CreateRuleRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    validation::validate_create_rule(&req)?;
    let rule = state.rule_repo.create(&req).await?;
    // No engine reload — drafts are not in the engine
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": RuleResponse::try_from(&rule)? })),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/rules/{id}",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    responses(
        (status = 200, description = "Rule details"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let rule = state.rule_repo.get_by_id(&id).await?;
    Ok(Json(json!({
        "data": RuleResponse::try_from(&rule)?,
    })))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/rules/{id}",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    request_body = UpdateRuleRequest,
    responses(
        (status = 200, description = "Draft rule updated"),
        (status = 400, description = "No draft version or invalid input"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn update_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRuleRequest>,
) -> Result<Json<Value>, OrionError> {
    validation::validate_update_rule(&req)?;
    let rule = state.rule_repo.update_draft(&id, &req).await?;
    // No engine reload — drafts are not in the engine
    Ok(Json(json!({ "data": RuleResponse::try_from(&rule)? })))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/rules/{id}",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    responses(
        (status = 204, description = "Rule deleted"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn delete_rule(
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

#[utoipa::path(
    patch,
    path = "/api/v1/admin/rules/{id}/status",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    request_body = StatusChangeRequest,
    responses(
        (status = 200, description = "Status updated"),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn change_rule_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let rule = match req.status.as_str() {
        "active" => {
            let rollout_pct = req.rollout_percentage.unwrap_or(100);
            state.rule_repo.activate(&id, rollout_pct).await?
        }
        "archived" => state.rule_repo.archive(&id).await?,
        other => {
            return Err(OrionError::BadRequest(format!(
                "Invalid status transition to '{}'. Use 'active' or 'archived'",
                other
            )));
        }
    };
    reload_engine(&state).await?;
    Ok(Json(json!({ "data": RuleResponse::try_from(&rule)? })))
}

// ============================================================
// Rule Rollout Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/rules/{id}/rollout",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    request_body = RolloutUpdateRequest,
    responses(
        (status = 200, description = "Rollout percentage updated"),
        (status = 400, description = "Invalid rollout configuration"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn update_rollout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RolloutUpdateRequest>,
) -> Result<Json<Value>, OrionError> {
    let rule = state
        .rule_repo
        .update_rollout(&id, req.rollout_percentage)
        .await?;
    reload_engine(&state).await?;
    Ok(Json(json!({ "data": RuleResponse::try_from(&rule)? })))
}

// ============================================================
// Rule Version Management
// ============================================================

#[derive(Debug, Deserialize)]
pub(crate) struct VersionFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/rules/{id}/versions",
    tag = "Rules",
    params(
        ("id" = String, Path, description = "Rule ID"),
    ),
    responses(
        (status = 200, description = "Paginated version history"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_rule_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(filter): Query<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    // Verify rule exists
    let _ = state.rule_repo.get_by_id(&id).await?;

    let limit = filter.limit.unwrap_or(50);
    let offset = filter.offset.unwrap_or(0);
    let result = state.rule_repo.list_versions(&id, limit, offset).await?;
    let data: Vec<RuleResponse> = result
        .data
        .iter()
        .map(RuleResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(json!({
        "data": data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rules/{id}/versions",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    responses(
        (status = 201, description = "New draft version created"),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn create_new_version(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let rule = state.rule_repo.create_new_version(&id).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": RuleResponse::try_from(&rule)? })),
    ))
}

// ============================================================
// Rule Dry-Run / Testing
// ============================================================

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct TestRuleRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rules/{id}/test",
    tag = "Rules",
    params(("id" = String, Path, description = "Rule ID")),
    request_body = TestRuleRequest,
    responses(
        (status = 200, description = "Test result with trace"),
        (status = 404, description = "Rule not found"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn test_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TestRuleRequest>,
) -> Result<Json<Value>, OrionError> {
    use crate::storage::repositories::rules::rule_to_workflow;

    let rule = state.rule_repo.get_by_id(&id).await?;
    let workflow = rule_to_workflow(&rule)?;

    // Create an isolated engine with just this one rule, reusing the shared HTTP client
    let custom_fns = crate::engine::build_custom_functions(
        state.connector_registry.clone(),
        state.http_client.clone(),
    );
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

    let trace_value = serde_json::to_value(&trace)?;

    Ok(Json(json!({
        "matched": matched,
        "trace": trace_value,
        "output": message.data(),
        "errors": message.errors.iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    })))
}

// ============================================================
// Rule Import / Export
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/rules/import",
    tag = "Rules",
    request_body = Vec<CreateRuleRequest>,
    responses(
        (status = 200, description = "Import results with counts"),
    )
)]
#[tracing::instrument(skip(state, rules), fields(count = rules.len()))]
pub(crate) async fn import_rules(
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

    // No engine reload — imported rules are drafts

    Ok(Json(json!({
        "imported": imported,
        "failed": failed,
        "errors": errors,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/rules/export",
    tag = "Rules",
    params(RuleFilter),
    responses(
        (status = 200, description = "Exported rules"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_rules(
    State(state): State<AppState>,
    Query(filter): Query<RuleFilter>,
) -> Result<Json<Value>, OrionError> {
    let rules = state.rule_repo.list(&filter).await?;
    let data: Vec<RuleResponse> = rules
        .iter()
        .map(RuleResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "data": data })))
}

// ============================================================
// Rule Validation
// ============================================================

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationIssue {
    field: String,
    message: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationResponse {
    valid: bool,
    errors: Vec<ValidationIssue>,
    warnings: Vec<ValidationIssue>,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rules/validate",
    tag = "Rules",
    request_body = CreateRuleRequest,
    responses(
        (status = 200, description = "Validation result", body = ValidationResponse),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn validate_rule(
    State(state): State<AppState>,
    Json(req): Json<CreateRuleRequest>,
) -> Result<Json<Value>, OrionError> {
    let result = run_validation(&req, &state).await;
    Ok(Json(json!({
        "valid": result.valid,
        "errors": result.errors,
        "warnings": result.warnings,
    })))
}

async fn run_validation(req: &CreateRuleRequest, state: &AppState) -> ValidationResponse {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // 1. Name non-empty
    if req.name.trim().is_empty() {
        errors.push(ValidationIssue {
            field: "name".to_string(),
            message: "Name cannot be empty".to_string(),
        });
    }

    // 2. Channel non-empty
    if req.channel.trim().is_empty() {
        errors.push(ValidationIssue {
            field: "channel".to_string(),
            message: "Channel cannot be empty".to_string(),
        });
    }

    // 3. Tasks is non-empty array
    let tasks = req.tasks.as_array();
    if tasks.is_none() || tasks.is_some_and(|t| t.is_empty()) {
        errors.push(ValidationIssue {
            field: "tasks".to_string(),
            message: "Tasks must be a non-empty array".to_string(),
        });
    }

    // Reuse a single DataLogic instance for all condition compilations
    let dl = datalogic_rs::DataLogic::new();

    // Task-level checks (4-5, 7-9)
    if let Some(tasks) = tasks {
        let mut seen_ids = HashSet::new();

        for (i, task) in tasks.iter().enumerate() {
            // 4. Each task has id, name, function.name
            let task_id = task.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if task_id.is_empty() {
                errors.push(ValidationIssue {
                    field: format!("tasks[{i}].id"),
                    message: format!("Task at index {i} is missing 'id'"),
                });
            }

            if task
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                errors.push(ValidationIssue {
                    field: format!("tasks[{i}].name"),
                    message: format!("Task at index {i} is missing 'name'"),
                });
            }

            let function = task.get("function");
            let fn_name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");

            if fn_name.is_empty() {
                errors.push(ValidationIssue {
                    field: format!("tasks[{i}].function.name"),
                    message: format!("Task at index {i} is missing 'function.name'"),
                });
            }

            // 5. Task IDs unique
            if !task_id.is_empty() && !seen_ids.insert(task_id) {
                errors.push(ValidationIssue {
                    field: "tasks".to_string(),
                    message: format!("Duplicate task id '{task_id}'"),
                });
            }

            // 7. Task conditions valid JSONLogic
            if let Some(condition) = task.get("condition") {
                if let Err(e) = dl.compile(condition) {
                    errors.push(ValidationIssue {
                        field: format!("tasks[{i}].condition"),
                        message: format!("Invalid JSONLogic in task condition: {e}"),
                    });
                }
            }

            // 8. Function name known
            if !fn_name.is_empty() && !crate::engine::KNOWN_FUNCTIONS.contains(&fn_name) {
                warnings.push(ValidationIssue {
                    field: format!("tasks[{i}].function.name"),
                    message: format!("Unknown function '{fn_name}'"),
                });
            }

            // 9. Connector ref exists
            if !fn_name.is_empty() && crate::engine::CONNECTOR_FUNCTIONS.contains(&fn_name) {
                if let Some(connector_name) = function
                    .and_then(|f| f.get("input"))
                    .and_then(|input| input.get("connector"))
                    .and_then(|c| c.as_str())
                {
                    if state.connector_registry.get(connector_name).await.is_none() {
                        warnings.push(ValidationIssue {
                            field: format!("tasks[{i}].function.input.connector"),
                            message: format!("Connector '{connector_name}' not found in registry"),
                        });
                    }
                }
            }
        }
    }

    // 6. Rule condition valid JSONLogic
    if let Err(e) = dl.compile(&req.condition) {
        errors.push(ValidationIssue {
            field: "condition".to_string(),
            message: format!("Invalid JSONLogic in rule condition: {e}"),
        });
    }

    // 10. Workflow conversion
    {
        use crate::storage::repositories::rules::rule_to_workflow;

        let temp_rule = crate::storage::models::Rule {
            rule_id: "temp-validate".to_string(),
            name: req.name.clone(),
            description: req.description.clone(),
            channel: req.channel.clone(),
            priority: req.priority,
            version: 1,
            status: "active".to_string(),
            rollout_percentage: 100,
            condition_json: serde_json::to_string(&req.condition).unwrap_or_else(|e| {
                errors.push(ValidationIssue {
                    field: "condition".to_string(),
                    message: format!("Failed to serialize condition: {e}"),
                });
                String::new()
            }),
            tasks_json: serde_json::to_string(&req.tasks).unwrap_or_else(|e| {
                errors.push(ValidationIssue {
                    field: "tasks".to_string(),
                    message: format!("Failed to serialize tasks: {e}"),
                });
                String::new()
            }),
            tags: serde_json::to_string(&req.tags).unwrap_or_else(|e| {
                errors.push(ValidationIssue {
                    field: "tags".to_string(),
                    message: format!("Failed to serialize tags: {e}"),
                });
                String::new()
            }),
            continue_on_error: req.continue_on_error,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };

        if let Err(e) = rule_to_workflow(&temp_rule) {
            errors.push(ValidationIssue {
                field: "(root)".to_string(),
                message: format!("Failed to convert to workflow: {e}"),
            });
        }
    }

    ValidationResponse {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
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

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors",
    tag = "Connectors",
    params(ConnectorFilter),
    responses(
        (status = 200, description = "Paginated list of connectors"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_connectors(
    State(state): State<AppState>,
    Query(filter): Query<ConnectorFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.connector_repo.list_paginated(&filter).await?;
    let masked: Vec<_> = result.data.iter().map(mask_connector).collect();
    Ok(Json(json!({
        "data": masked,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors",
    tag = "Connectors",
    request_body = CreateConnectorRequest,
    responses(
        (status = 201, description = "Connector created"),
        (status = 409, description = "Connector name conflict"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn create_connector(
    State(state): State<AppState>,
    Json(req): Json<CreateConnectorRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    validation::validate_create_connector(&req)?;
    let connector = state.connector_repo.create(&req).await?;
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok((StatusCode::CREATED, Json(json!({ "data": masked }))))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    responses(
        (status = 200, description = "Connector details"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let connector = state.connector_repo.get_by_id(&id).await?;
    let masked = mask_connector(&connector);
    Ok(Json(json!({ "data": masked })))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    request_body = UpdateConnectorRequest,
    responses(
        (status = 200, description = "Connector updated"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn update_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConnectorRequest>,
) -> Result<Json<Value>, OrionError> {
    validation::validate_update_connector(&req)?;
    let connector = state.connector_repo.update(&id, &req).await?;
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok(Json(json!({ "data": masked })))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    responses(
        (status = 204, description = "Connector deleted"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn delete_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.connector_repo.delete(&id).await?;
    reload_connectors(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Circuit Breakers
// ============================================================

#[tracing::instrument(skip(state))]
pub(crate) async fn list_circuit_breakers(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let states = state.connector_registry.circuit_breaker_states().await;
    Ok(Json(json!({
        "enabled": state.connector_registry.circuit_breaker_enabled(),
        "breakers": states,
    })))
}

#[tracing::instrument(skip(state))]
pub(crate) async fn reset_circuit_breaker(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let found = state.connector_registry.reset_circuit_breaker(&key).await;
    if found {
        Ok(Json(json!({ "reset": true, "key": key })))
    } else {
        Err(OrionError::NotFound(format!(
            "Circuit breaker '{}' not found",
            key
        )))
    }
}

// ============================================================
// Engine Control
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/engine/status",
    tag = "Engine",
    responses(
        (status = 200, description = "Engine status"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn engine_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let engine = state.engine.read().await;
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

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "rules_count": workflows.len(),
        "active_rules": active_count,
        "channels": channels.into_iter().collect::<Vec<_>>(),
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/engine/reload",
    tag = "Engine",
    responses(
        (status = 200, description = "Engine reloaded"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn engine_reload(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    reload_engine(&state).await?;

    let engine = state.engine.read().await;
    let rules_count = engine.workflows().len();

    Ok(Json(json!({
        "reloaded": true,
        "rules_count": rules_count,
    })))
}
