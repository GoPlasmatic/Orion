use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::response_helpers::{
    created_response, data_response, paginated_response,
};
use crate::server::state::AppState;
use crate::storage::models::{StatusAction, WorkflowResponse};
use crate::storage::repositories::workflows::{
    CreateWorkflowRequest, RolloutUpdateRequest, StatusChangeRequest, UpdateWorkflowRequest,
    WorkflowFilter,
};

use super::VersionFilter;
use super::audit_log;
use super::reload_engine;

// ============================================================
// Workflows CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows",
    params(WorkflowFilter),
    tag = "Workflows",
    responses(
        (status = 200, description = "Paginated list of workflows"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_workflows(
    State(state): State<AppState>,
    Query(filter): Query<WorkflowFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.workflow_repo.list_paginated(&filter).await?;
    let data: Vec<WorkflowResponse> = result
        .data
        .iter()
        .map(WorkflowResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(paginated_response(
        data,
        result.total,
        result.limit,
        result.offset,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows",
    tag = "Workflows",
    request_body = CreateWorkflowRequest,
    responses(
        (status = 201, description = "Workflow created as draft"),
        (status = 400, description = "Invalid input"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_workflow(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<CreateWorkflowRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_workflow(&req)?;
    let workflow = state.workflow_repo.create(&req).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create",
        "workflow",
        &workflow.workflow_id,
    );
    // No engine reload — drafts are not in the engine
    Ok(created_response(WorkflowResponse::try_from(&workflow)?))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows/{id}",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    responses(
        (status = 200, description = "Workflow details"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let workflow = state.workflow_repo.get_by_id(&id).await?;
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/workflows/{id}",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    request_body = UpdateWorkflowRequest,
    responses(
        (status = 200, description = "Draft workflow updated"),
        (status = 400, description = "No draft version or invalid input"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_workflow(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateWorkflowRequest>,
) -> Result<Json<Value>, OrionError> {
    crate::validation::validate_update_workflow(&req)?;
    let workflow = state.workflow_repo.update_draft(&id, &req).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "update",
        "workflow",
        &id,
    );
    // No engine reload — drafts are not in the engine
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/workflows/{id}",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    responses(
        (status = 204, description = "Workflow deleted"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_workflow(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.workflow_repo.delete(&id).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "delete",
        "workflow",
        &id,
    );
    reload_engine(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Workflow Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/workflows/{id}/status",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    request_body = StatusChangeRequest,
    responses(
        (status = 200, description = "Status updated"),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_workflow_status(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status.as_str())?;
    let workflow = match action {
        StatusAction::Activate => {
            let rollout_pct = req.rollout_percentage.unwrap_or(100);
            state.workflow_repo.activate(&id, rollout_pct).await?
        }
        StatusAction::Archive => state.workflow_repo.archive(&id).await?,
    };
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        &format!("status_{}", req.status),
        "workflow",
        &id,
    );
    reload_engine(&state).await?;
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

// ============================================================
// Workflow Rollout Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/workflows/{id}/rollout",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    request_body = RolloutUpdateRequest,
    responses(
        (status = 200, description = "Rollout percentage updated"),
        (status = 400, description = "Invalid rollout configuration"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_rollout(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<RolloutUpdateRequest>,
) -> Result<Json<Value>, OrionError> {
    let workflow = state
        .workflow_repo
        .update_rollout(&id, req.rollout_percentage)
        .await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "update_rollout",
        "workflow",
        &id,
    );
    reload_engine(&state).await?;
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

// ============================================================
// Workflow Version Management
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows/{id}/versions",
    tag = "Workflows",
    params(
        ("id" = String, Path, description = "Workflow ID"),
    ),
    responses(
        (status = 200, description = "Paginated version history"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_workflow_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(filter): Query<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    // Verify workflow exists
    let _ = state.workflow_repo.get_by_id(&id).await?;

    let limit = filter.limit.unwrap_or(50);
    let offset = filter.offset.unwrap_or(0);
    let result = state
        .workflow_repo
        .list_versions(&id, limit, offset)
        .await?;
    let data: Vec<WorkflowResponse> = result
        .data
        .iter()
        .map(WorkflowResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(paginated_response(
        data,
        result.total,
        result.limit,
        result.offset,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows/{id}/versions",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    responses(
        (status = 201, description = "New draft version created"),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_new_workflow_version(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let workflow = state.workflow_repo.create_new_version(&id).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create_version",
        "workflow",
        &id,
    );
    Ok(created_response(WorkflowResponse::try_from(&workflow)?))
}

// ============================================================
// Workflow Dry-Run / Testing
// ============================================================

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct TestWorkflowRequest {
    data: Value,
    #[serde(default)]
    metadata: Value,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows/{id}/test",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    request_body = TestWorkflowRequest,
    responses(
        (status = 200, description = "Test result with trace"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn test_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TestWorkflowRequest>,
) -> Result<Json<Value>, OrionError> {
    use crate::storage::repositories::workflows::workflow_to_dataflow;

    let workflow = state.workflow_repo.get_by_id(&id).await?;
    let df_workflow = workflow_to_dataflow(&workflow, "__test__")?;

    // Create an isolated engine with just this one workflow, reusing the shared HTTP client.
    // channel_call in dry-run still routes through the main engine for cross-channel calls.
    let custom_fns = crate::engine::build_custom_functions(
        state.connector_registry.clone(),
        state.http_client.clone(),
        state.engine.clone(),
        &state.config.engine,
        state.cache_pool.clone(),
    );
    let test_engine = dataflow_rs::Engine::new(vec![df_workflow], Some(custom_fns));

    let mut payload = json!({});
    if let Some(obj) = req.data.as_object() {
        for (k, v) in obj {
            payload[k] = v.clone();
        }
    } else {
        payload = req.data;
    }

    let mut message = dataflow_rs::Message::from_value(&payload);
    super::super::data::merge_metadata(&mut message, &req.metadata);

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
// Workflow Import / Export
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows/import",
    tag = "Workflows",
    request_body = Vec<CreateWorkflowRequest>,
    responses(
        (status = 200, description = "Import results with counts"),
    )
)]
#[tracing::instrument(skip(state, workflows, principal), fields(count = workflows.len()))]
pub(crate) async fn import_workflows(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(workflows): Json<Vec<CreateWorkflowRequest>>,
) -> Result<Json<Value>, OrionError> {
    let results = state.workflow_repo.bulk_create(&workflows).await?;

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

    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "import",
        "workflow",
        &format!("{imported} imported"),
    );

    // No engine reload — imported workflows are drafts

    Ok(Json(json!({
        "imported": imported,
        "failed": failed,
        "errors": errors,
    })))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows/export",
    tag = "Workflows",
    params(WorkflowFilter),
    responses(
        (status = 200, description = "Exported workflows"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_workflows(
    State(state): State<AppState>,
    Query(filter): Query<WorkflowFilter>,
) -> Result<Json<Value>, OrionError> {
    let workflows = state.workflow_repo.list(&filter).await?;
    let data: Vec<WorkflowResponse> = workflows
        .iter()
        .map(WorkflowResponse::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(data_response(data))
}

// ============================================================
// Workflow Validation
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
    path = "/api/v1/admin/workflows/validate",
    tag = "Workflows",
    request_body = CreateWorkflowRequest,
    responses(
        (status = 200, description = "Validation result", body = ValidationResponse),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn validate_workflow(
    State(state): State<AppState>,
    Json(req): Json<CreateWorkflowRequest>,
) -> Result<Json<Value>, OrionError> {
    let result = run_validation(&req, &state).await;
    Ok(Json(json!({
        "valid": result.valid,
        "errors": result.errors,
        "warnings": result.warnings,
    })))
}

async fn run_validation(req: &CreateWorkflowRequest, state: &AppState) -> ValidationResponse {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    validate_basic_fields(req, &mut errors);

    let dl = datalogic_rs::DataLogic::new();

    if let Some(tasks) = req.tasks.as_array() {
        validate_tasks(tasks, &dl, state, &mut errors, &mut warnings).await;
    }

    validate_workflow_condition(&req.condition, &dl, &mut errors);
    validate_dataflow_conversion(req, &mut errors);

    ValidationResponse {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

/// Validate name and tasks are non-empty.
fn validate_basic_fields(req: &CreateWorkflowRequest, errors: &mut Vec<ValidationIssue>) {
    if req.name.trim().is_empty() {
        errors.push(ValidationIssue {
            field: "name".to_string(),
            message: "Name cannot be empty".to_string(),
        });
    }
    let tasks = req.tasks.as_array();
    if tasks.is_none() || tasks.is_some_and(|t| t.is_empty()) {
        errors.push(ValidationIssue {
            field: "tasks".to_string(),
            message: "Tasks must be a non-empty array".to_string(),
        });
    }
}

/// Validate individual tasks: required fields, unique IDs, conditions, function names, connector refs.
async fn validate_tasks(
    tasks: &[Value],
    dl: &datalogic_rs::DataLogic,
    state: &AppState,
    errors: &mut Vec<ValidationIssue>,
    warnings: &mut Vec<ValidationIssue>,
) {
    let mut seen_ids = HashSet::new();

    for (i, task) in tasks.iter().enumerate() {
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

        if !task_id.is_empty() && !seen_ids.insert(task_id) {
            errors.push(ValidationIssue {
                field: "tasks".to_string(),
                message: format!("Duplicate task id '{task_id}'"),
            });
        }

        if let Some(condition) = task.get("condition")
            && let Err(e) = dl.compile(condition)
        {
            errors.push(ValidationIssue {
                field: format!("tasks[{i}].condition"),
                message: format!("Invalid JSONLogic in task condition: {e}"),
            });
        }

        if !fn_name.is_empty() && !crate::engine::KNOWN_FUNCTIONS.contains(&fn_name) {
            warnings.push(ValidationIssue {
                field: format!("tasks[{i}].function.name"),
                message: format!("Unknown function '{fn_name}'"),
            });
        }

        if !fn_name.is_empty()
            && crate::engine::CONNECTOR_FUNCTIONS.contains(&fn_name)
            && let Some(connector_name) = function
                .and_then(|f| f.get("input"))
                .and_then(|input| input.get("connector"))
                .and_then(|c| c.as_str())
            && state.connector_registry.get(connector_name).await.is_none()
        {
            warnings.push(ValidationIssue {
                field: format!("tasks[{i}].function.input.connector"),
                message: format!("Connector '{connector_name}' not found in registry"),
            });
        }
    }
}

/// Validate workflow-level JSONLogic condition.
fn validate_workflow_condition(
    condition: &Value,
    dl: &datalogic_rs::DataLogic,
    errors: &mut Vec<ValidationIssue>,
) {
    if let Err(e) = dl.compile(condition) {
        errors.push(ValidationIssue {
            field: "condition".to_string(),
            message: format!("Invalid JSONLogic in workflow condition: {e}"),
        });
    }
}

/// Validate that the workflow can be converted to a dataflow-rs workflow.
fn validate_dataflow_conversion(req: &CreateWorkflowRequest, errors: &mut Vec<ValidationIssue>) {
    use crate::storage::repositories::workflows::workflow_to_dataflow;

    let temp_workflow = crate::storage::models::Workflow {
        workflow_id: "temp-validate".to_string(),
        name: req.name.clone(),
        description: req.description.clone(),
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

    if let Err(e) = workflow_to_dataflow(&temp_workflow, "__validate__") {
        errors.push(ValidationIssue {
            field: "(root)".to_string(),
            message: format!("Failed to convert to dataflow workflow: {e}"),
        });
    }
}
