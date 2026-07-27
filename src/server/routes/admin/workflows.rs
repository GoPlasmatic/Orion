use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionJson;
use crate::server::routes::response_helpers::{created_response, data_response, paginated_into};
use crate::server::state::AppState;
use crate::storage::models::{StatusAction, WorkflowResponse};
use crate::storage::repositories::workflows::{
    CreateWorkflowRequest, RolloutUpdateRequest, StatusChangeRequest, UpdateWorkflowRequest,
    WorkflowFilter,
};

use super::VersionFilter;
use super::audit_and_reload;
use super::audit_log_draft_only;

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
    paginated_into(result, |w| WorkflowResponse::try_from(w))
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
    OrionJson(req): OrionJson<CreateWorkflowRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_workflow(&req)?;
    let workflow = state.workflow_repo.create(&req).await?;
    audit_log_draft_only(
        &state.audit_log_repo,
        &principal,
        "create",
        "workflow",
        &workflow.workflow_id,
    );
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
    OrionJson(req): OrionJson<UpdateWorkflowRequest>,
) -> Result<Json<Value>, OrionError> {
    crate::validation::validate_update_workflow(&req)?;
    let workflow = state.workflow_repo.update_draft(&id, &req).await?;
    audit_log_draft_only(&state.audit_log_repo, &principal, "update", "workflow", &id);
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
    audit_and_reload(&state, &principal, "delete", "workflow", &id).await?;
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
    let action = StatusAction::parse(req.status)?;
    let workflow = match action {
        StatusAction::Activate => {
            // R5: refuse to activate a workflow that cannot run. Connector
            // references stay a warning at create time (connectors and
            // workflows may be authored in either order) — activation is
            // the gate, because from here the workflow serves traffic and
            // a missing connector is a guaranteed runtime 500.
            let draft = state.workflow_repo.get_by_id(&id).await?;
            ensure_workflow_connectors_exist(&state, &draft).await?;
            let rollout_pct = req.rollout_percentage.unwrap_or(100);
            state.workflow_repo.activate(&id, rollout_pct).await?
        }
        StatusAction::Archive => state.workflow_repo.archive(&id).await?,
    };
    audit_and_reload(
        &state,
        &principal,
        &format!("status_{}", req.status),
        "workflow",
        &id,
    )
    .await?;
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

/// R5: every connector a workflow's tasks reference must exist before the
/// workflow may activate. Missing connectors were previously a warning at
/// create and unchecked at activate, so the workflow failed at its first
/// request instead.
async fn ensure_workflow_connectors_exist(
    state: &AppState,
    workflow: &crate::storage::models::Workflow,
) -> Result<(), OrionError> {
    let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
        return Ok(()); // unparseable tasks are caught elsewhere
    };
    let Some(tasks) = tasks.as_array() else {
        return Ok(());
    };
    let mut missing = Vec::new();
    for task in tasks {
        let function = task.get("function");
        let fn_name = function
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if !crate::engine::CONNECTOR_FUNCTIONS.contains(&fn_name) {
            continue;
        }
        if let Some(connector) = function
            .and_then(|f| f.get("input"))
            .and_then(|i| i.get("connector"))
            .and_then(|c| c.as_str())
            && state.connector_registry.get(connector).await.is_none()
            && !missing.contains(&connector.to_string())
        {
            missing.push(connector.to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(OrionError::validation(format!(
            "Cannot activate workflow '{}': connector(s) {} not found — create \
             them first, or fix the reference",
            workflow.workflow_id,
            missing
                .iter()
                .map(|m| format!("'{m}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
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
    audit_and_reload(&state, &principal, "update_rollout", "workflow", &id).await?;
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
    paginated_into(result, |w| WorkflowResponse::try_from(w))
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
    audit_log_draft_only(
        &state.audit_log_repo,
        &principal,
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
        state.channel_registry.clone(),
        &state.config.engine,
        &state.config.query,
        &state.config.write,
        state.cache_pool.clone(),
        state.sql_pool_cache.clone(),
        state.mongo_pool_cache.clone(),
    );
    let test_engine =
        dataflow_rs::Engine::new(vec![df_workflow], custom_fns).map_err(OrionError::Engine)?;

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
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    })))
}

// ============================================================
// Workflow Import / Export
// ============================================================

/// Query parameters accepted by all three /import endpoints (B6).
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
pub(crate) struct ImportQuery {
    /// When true, validates each item and reports what would happen
    /// without writing to the database. The response shape mirrors
    /// a real import but `imported` is always 0.
    #[serde(default)]
    pub dry_run: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows/import",
    tag = "Workflows",
    request_body = Vec<CreateWorkflowRequest>,
    params(ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true)"),
    )
)]
#[tracing::instrument(skip(state, workflows, principal), fields(count = workflows.len()))]
pub(crate) async fn import_workflows(
    State(state): State<AppState>,
    Query(query): Query<ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(workflows): Json<Vec<CreateWorkflowRequest>>,
) -> Result<Json<Value>, OrionError> {
    if query.dry_run {
        // Validate each item against the same checks the create endpoint
        // would run; never touch the DB. Useful for CI: check that a
        // bulk export still loads cleanly without mutating state.
        let mut would_create = 0u64;
        let mut would_fail = 0u64;
        let mut errors = Vec::new();
        for (i, wf) in workflows.iter().enumerate() {
            match crate::validation::validate_create_workflow(wf) {
                Ok(()) => would_create += 1,
                Err(e) => {
                    would_fail += 1;
                    errors.push(json!({
                        "index": i,
                        "error": e.to_string(),
                    }));
                }
            }
        }
        return Ok(Json(json!({
            "dry_run": true,
            "would_create": would_create,
            "would_fail": would_fail,
            "imported": 0,
            "failed": would_fail,
            "errors": errors,
        })));
    }

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

    audit_log_draft_only(
        &state.audit_log_repo,
        &principal,
        "import",
        "workflow",
        &format!("{imported} imported"),
    );

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
    OrionJson(req): OrionJson<CreateWorkflowRequest>,
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

    let dl = datalogic_rs::Engine::new();

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

/// Validate all tasks. Walks the task list once, delegating per-task checks
/// to [`errors_for_task`] and tracking cross-task duplicate IDs here.
async fn validate_tasks(
    tasks: &[Value],
    dl: &datalogic_rs::Engine,
    state: &AppState,
    errors: &mut Vec<ValidationIssue>,
    warnings: &mut Vec<ValidationIssue>,
) {
    let mut seen_ids: HashSet<&str> = HashSet::new();

    for (i, task) in tasks.iter().enumerate() {
        let (task_errors, task_warnings) = errors_for_task(i, task, dl, state).await;
        errors.extend(task_errors);
        warnings.extend(task_warnings);

        // Cross-task check: duplicate task IDs.
        let task_id = task.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !task_id.is_empty() && !seen_ids.insert(task_id) {
            errors.push(ValidationIssue {
                field: "tasks".to_string(),
                message: format!("Duplicate task id '{task_id}'"),
            });
        }
    }
}

/// All per-task validations (required fields, condition, function name,
/// schema, connector reference). Returns `(errors, warnings)`.
async fn errors_for_task(
    i: usize,
    task: &Value,
    dl: &datalogic_rs::Engine,
    state: &AppState,
) -> (Vec<ValidationIssue>, Vec<ValidationIssue>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

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

    // Schema check the function input (A1). Same registry is used by
    // workflow create — surfacing it here so the /validate endpoint
    // gives the same answer offline.
    if !fn_name.is_empty() {
        let input = function
            .and_then(|f| f.get("input"))
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let task_path = format!("tasks[{i}]");
        for fe in crate::engine::functions::schema::validate_input(fn_name, &input, &task_path) {
            errors.push(ValidationIssue {
                field: fe.path,
                message: fe.message,
            });
        }
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

    (errors, warnings)
}

/// Validate workflow-level JSONLogic condition.
fn validate_workflow_condition(
    condition: &Value,
    dl: &datalogic_rs::Engine,
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
        status: crate::storage::models::EntityStatus::Active
            .as_str()
            .to_string(),
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
