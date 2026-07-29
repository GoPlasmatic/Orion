use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{
    DataEnvelope, ImportResult, PaginatedEnvelope, WorkflowTestResult,
};
use crate::server::routes::response_helpers::{created_response, data_response, paginated_into};
use crate::server::state::AppState;
use crate::storage::models::WorkflowResponse;
use crate::storage::repositories::workflows::{
    CreateWorkflowRequest, RolloutUpdateRequest, StatusChangeRequest, UpdateWorkflowRequest,
    WorkflowFilter,
};

use super::StatusAction;
use super::VersionFilter;
use super::audit_and_reload;
use super::audit_log;
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
        (status = 200, description = "Paginated list of workflows", body = PaginatedEnvelope<WorkflowResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_workflows(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<WorkflowFilter>,
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
        (status = 201, description = "Workflow created as draft", body = DataEnvelope<WorkflowResponse>),
        (status = 400, description = "Invalid input"),
        (status = 409, description = "Workflow id already exists"),
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
        &state.audit_queue,
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
        (status = 200, description = "Workflow details", body = DataEnvelope<WorkflowResponse>),
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
        (status = 200, description = "Draft workflow updated", body = DataEnvelope<WorkflowResponse>),
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
    audit_log_draft_only(&state.audit_queue, &principal, "update", "workflow", &id);
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
        (status = 200, description = "Status updated", body = DataEnvelope<WorkflowResponse>),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_workflow_status(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<StatusChangeRequest>,
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

/// One task's connector reference, as authored.
pub(crate) struct ConnectorRef<'a> {
    /// The task's `function.name`, always one of `CONNECTOR_FUNCTIONS`.
    pub function: &'a str,
    pub connector: &'a str,
    /// The task's whole `function.input` object, for the cross-field rules.
    pub input: &'a Value,
}

/// Every connector a workflow's tasks reference, in task order.
///
/// Shared by the activation check below and the connector-rename guard in
/// `admin/connectors.rs` (F18), which needs the same walk to answer "is any
/// active workflow pointing at this name?".
pub(crate) fn connector_refs(tasks: &Value) -> Vec<ConnectorRef<'_>> {
    let Some(tasks) = tasks.as_array() else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter_map(|task| {
            let function = task.get("function")?;
            let name = function.get("name")?.as_str()?;
            if !crate::engine::CONNECTOR_FUNCTIONS.contains(&name) {
                return None;
            }
            let input = function.get("input")?;
            Some(ConnectorRef {
                function: name,
                connector: input.get("connector")?.as_str()?,
                input,
            })
        })
        .collect()
}

/// R5 / F52: every connector a workflow's tasks reference must exist, be of a
/// type the referencing function can actually use, and carry the extra keys
/// that type needs — all before the workflow may activate.
///
/// Missing connectors were previously a warning at create and unchecked at
/// activate, so the workflow failed at its first request instead (R5). The
/// *type* stayed unchecked even then: pointing `cache_read` at a `db`
/// connector activated cleanly and 500'd on first traffic, though
/// `CONNECTOR_FUNCTIONS` already implied the required kind (F52). Both are
/// fully determined at authoring time, so both are answered here.
async fn ensure_workflow_connectors_exist(
    state: &AppState,
    workflow: &crate::storage::models::Workflow,
) -> Result<(), OrionError> {
    let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
        return Ok(()); // unparseable tasks are caught elsewhere
    };
    let mut missing: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    for task in connector_refs(&tasks) {
        let Some(config) = state.connector_registry.get(task.connector).await else {
            if !missing.contains(&task.connector.to_string()) {
                missing.push(task.connector.to_string());
            }
            continue;
        };

        // Type. The handler would refuse this at request time via
        // `require_*_connector`; saying so now costs one registry read.
        let actual = config.connector_type();
        if let Some(wanted) = crate::engine::required_connector_types(task.function)
            && !wanted.contains(&actual)
        {
            problems.push(format!(
                "task calling '{}' points at connector '{}', which is a '{actual}' \
                 connector — '{}' requires {}",
                task.function,
                task.connector,
                task.function,
                wanted
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
            continue;
        }

        // Cross-field: MongoDB has no default database in its connection
        // string, so the task must name one. `mongo_read` declares `database`
        // required outright; `data_query`/`data_write` cannot, because the same
        // shape is valid against SQL and Elasticsearch — which is why the
        // schema marks it optional and the handler enforces it at request time.
        // Whether it applies is knowable here: the connector is already
        // resolved.
        if config.is_mongo()
            && crate::engine::requires_mongo_database(task.function)
            && !task
                .input
                .get("database")
                .is_some_and(|d| d.as_str().is_some_and(|s| !s.trim().is_empty()))
        {
            problems.push(format!(
                "task calling '{}' points at MongoDB connector '{}' but sets no \
                 'database' — MongoDB connection strings carry no default database",
                task.function, task.connector
            ));
        }
    }

    if !missing.is_empty() {
        return Err(OrionError::validation(format!(
            "Cannot activate workflow '{}': connector(s) {} not found — create \
             them first, or fix the reference",
            workflow.workflow_id,
            missing
                .iter()
                .map(|m| format!("'{m}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !problems.is_empty() {
        return Err(OrionError::validation(format!(
            "Cannot activate workflow '{}': {}",
            workflow.workflow_id,
            problems.join("; ")
        )));
    }
    Ok(())
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
        (status = 200, description = "Rollout percentage updated", body = DataEnvelope<WorkflowResponse>),
        (status = 400, description = "Invalid rollout configuration"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_rollout(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<RolloutUpdateRequest>,
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
        (status = 200, description = "Paginated version history", body = PaginatedEnvelope<WorkflowResponse>),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_workflow_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(filter): OrionQuery<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    // Verify workflow exists
    let _ = state.workflow_repo.get_by_id(&id).await?;

    let (limit, offset) = filter.limit_offset();
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
        (status = 201, description = "New draft version created", body = DataEnvelope<WorkflowResponse>),
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
        &state.audit_queue,
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
        (status = 200, description = "Test result with trace", body = DataEnvelope<WorkflowTestResult>),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn test_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<TestWorkflowRequest>,
) -> Result<Json<Value>, OrionError> {
    use crate::storage::repositories::workflows::workflow_to_dataflow;

    let workflow = state.workflow_repo.get_by_id(&id).await?;

    // O7: this endpoint runs the workflow's tasks against **live connectors** —
    // it will POST to real webhooks, write to real databases and publish to
    // real topics. It read as a harmless dry run and emitted no audit event at
    // all, so the most side-effecting call on the admin plane was the one
    // operation the trail could not show. Recorded before execution, so the
    // attempt is on the record even if the run itself fails or the process
    // dies mid-task.
    audit_log(&state.audit_queue, &principal, "test", "workflow", &id);

    let df_workflow = workflow_to_dataflow(&workflow, "__test__")?;

    // Create an isolated engine with just this one workflow, reusing the shared HTTP client.
    // channel_call in dry-run still routes through the main engine for cross-channel calls.
    let custom_fns =
        crate::engine::build_custom_functions(crate::engine::HandlerDeps::from_state(&state));
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
    crate::engine::utils::merge_metadata(&mut message, &req.metadata);

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

    Ok(data_response(json!({
        "matched": matched,
        "trace": trace_value,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
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
    params(super::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Each item is handled independently: a malformed or conflicting item becomes one entry in \
            `errors` and the rest of the batch still applies. Dry-run additionally probes for name \
            conflicts against stored rows and duplicates within the batch, without writing.", body = DataEnvelope<ImportResult>),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_workflows(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    // R19: the same per-item driver as channels and connectors. This endpoint
    // used to drive `bulk_create` over a typed `Vec<CreateWorkflowRequest>`, so
    // one malformed item aborted the whole batch with a 400 while its two
    // siblings reported it as a single failed entry.
    super::check_import_batch_size(items.len())?;
    let repo = state.workflow_repo.clone();
    let probe = state.workflow_repo.clone();
    let (imported, failed, errors) =
        super::import_items::<CreateWorkflowRequest, _, _, _, _, _, _>(
            items,
            query.dry_run,
            super::ImportOps {
                validate: crate::validation::validate_create_workflow,
                conflict_key: |w: &CreateWorkflowRequest| w.workflow_id.clone(),
                exists: |id: String| {
                    let repo = probe.clone();
                    async move { exists_or_err(repo.get_by_id(&id).await) }
                },
                create: |w: CreateWorkflowRequest| {
                    let repo = repo.clone();
                    async move { repo.create(&w).await.map(|_| ()) }
                },
            },
        )
        .await;
    if query.dry_run {
        return Ok(super::dry_run_response(imported, failed, errors));
    }

    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "workflow",
        &format!("{imported} imported"),
    );

    Ok(super::import_response(imported, failed, errors))
}

/// Turn a `get_by_id` result into an existence answer.
///
/// `NotFound` is the "no conflict" answer, not a failure; anything else is a
/// probe that could not run, which must not be reported as a clean item (R15).
pub(crate) fn exists_or_err<T>(result: Result<T, OrionError>) -> Result<bool, OrionError> {
    match result {
        Ok(_) => Ok(true),
        Err(OrionError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Rows per repository call on the export path (D7). Export still returns
/// everything that matches, but never asks the database for more than this
/// in one query.
const EXPORT_PAGE_SIZE: i64 = 500;

/// Fetch every workflow matching `filter`, `page_size` rows per repository
/// call, looping until a short page says the table is exhausted (D7).
///
/// The filter's own `limit`/`offset` are ignored, as export always has:
/// its contract is "everything that matches", and the paging here is an
/// implementation bound, not a client window.
///
/// Invariant: `page_size` must lie in `1..=1000` — the repository clamps the
/// limit it is handed (`clamp_pagination`), so a larger request comes back
/// as at most 1000 rows, the short-page check misreads that as "exhausted",
/// and the export silently truncates. Enforced by a `debug_assert!` below.
///
/// Not a snapshot: each page is an independent query with no transaction
/// spanning them, so workflows mutated concurrently between pages can be
/// skipped or duplicated within a single export response. (The previous
/// one-query export was per-statement consistent; bounded paging trades
/// that away.) The `workflow_id` tiebreaker only fixes ordering
/// nondeterminism, not cross-page mutation.
pub(crate) async fn collect_export_pages(
    repo: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    filter: &WorkflowFilter,
    page_size: i64,
) -> Result<Vec<WorkflowResponse>, OrionError> {
    debug_assert!(
        (1..=1000).contains(&page_size),
        "page_size {page_size} is outside the repository clamp (1..=1000); \
         a clamped page would silently truncate the export"
    );
    let mut data = Vec::new();
    let mut offset = 0i64;
    loop {
        let page_filter = WorkflowFilter {
            status: filter.status.clone(),
            tag: filter.tag.clone(),
            limit: Some(page_size),
            offset: Some(offset),
            ..Default::default()
        };
        let page = repo.list(&page_filter).await?;
        let page_len = page.len() as i64;
        for workflow in &page {
            data.push(WorkflowResponse::try_from(workflow)?);
        }
        if page_len < page_size {
            return Ok(data);
        }
        offset += page_size;
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows/export",
    tag = "Workflows",
    params(WorkflowFilter),
    responses(
        (status = 200, description = "Exported workflows", body = DataEnvelope<Vec<WorkflowResponse>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_workflows(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<WorkflowFilter>,
) -> Result<Json<Value>, OrionError> {
    let data =
        collect_export_pages(state.workflow_repo.as_ref(), &filter, EXPORT_PAGE_SIZE).await?;
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

/// The `{"data": …}` envelope (R17) around a [`ValidationResponse`]. Typed
/// rather than a `json!` literal so the declared `body =` below cannot drift
/// from what the handler actually sends.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ValidationEnvelope {
    data: ValidationResponse,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/workflows/validate",
    tag = "Workflows",
    request_body = CreateWorkflowRequest,
    responses(
        (status = 200, description = "Validation result", body = ValidationEnvelope),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn validate_workflow(
    State(state): State<AppState>,
    OrionJson(req): OrionJson<CreateWorkflowRequest>,
) -> Result<Json<ValidationEnvelope>, OrionError> {
    let data = run_validation(&req, &state).await;
    Ok(Json(ValidationEnvelope { data }))
}

/// R20: `valid: true` must mean `POST /api/v1/admin/workflows` would accept
/// this payload.
///
/// It did not. `validate_workflow_tasks_schema` carried the doc comment *"Public
/// so the `/validate` endpoint can reuse it"* and had **zero external callers**;
/// this function re-implemented the same walk, and the two disagreed by design —
/// an unknown `function.name` was a hard error on the create path and a
/// *warning* here. So `/validate` reported `valid: true` for a workflow create
/// would reject, which is worse than having no linter: it is a linter that lies
/// in the one direction that matters.
///
/// The create-path validator now runs first and verbatim, so agreement is
/// structural rather than maintained by hand. What follows it is only ever
/// *additional*: checks create does not make but activation does (a task with no
/// `id`, a condition that will not compile, a workflow that cannot be converted),
/// plus genuinely advisory warnings. Stricter is the safe direction for a
/// pre-flight tool; laxer is the bug.
async fn run_validation(req: &CreateWorkflowRequest, state: &AppState) -> ValidationResponse {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if let Err(e) = crate::validation::validate_create_workflow(req) {
        errors.extend(issues_from_error(e));
    }

    validate_task_array_shape(req, &mut errors);

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

/// Render an `OrionError` from the create path as `/validate` issues, keeping
/// the per-field detail where there is any.
fn issues_from_error(err: OrionError) -> Vec<ValidationIssue> {
    match err {
        OrionError::Validation { details, .. } if !details.is_empty() => details
            .into_iter()
            .map(|d| ValidationIssue {
                field: d.path,
                message: d.message,
            })
            .collect(),
        other => vec![ValidationIssue {
            field: "(root)".to_string(),
            message: other.client_message(),
        }],
    }
}

/// `tasks` must be a non-empty array. Create does not check this — a workflow
/// with no tasks stores fine and matches nothing — but a linter should say so.
fn validate_task_array_shape(req: &CreateWorkflowRequest, errors: &mut Vec<ValidationIssue>) {
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

    // R20: the unknown-function check and the input-schema walk used to be
    // re-implemented here, and the unknown-function copy reported a *warning*
    // where create reports an error. Both now come from
    // `validate_create_workflow`, run once in `run_validation` — so this
    // function is left with only the checks create does not make.

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
    use crate::storage::repositories::workflows::{synthetic_workflow, workflow_to_dataflow};

    match synthetic_workflow(req, "temp-validate") {
        Ok(w) => {
            if let Err(e) = workflow_to_dataflow(&w, "__validate__") {
                errors.push(ValidationIssue {
                    field: "(root)".to_string(),
                    message: format!("Failed to convert to dataflow workflow: {e}"),
                });
            }
        }
        // Serializing request fields back to JSON cannot realistically fail,
        // but surface it rather than swallow it.
        Err(e) => errors.push(ValidationIssue {
            field: "(root)".to_string(),
            message: format!("Failed to serialize workflow fields: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

    /// D7 regression: export must page through the repository in bounded
    /// pages and still return every matching workflow exactly once. With a
    /// page size of 2 and 5 rows this loops three times; if the loop stops
    /// after the first page, or `list` reverts to ignoring its filter, an
    /// assertion fails. The `timeout` matters: with `list` ignoring its
    /// limit, every page returns all 5 rows (never a short page), so an
    /// unbounded call would spin forever — the revert must show up as a red
    /// test, not a hang. The pre-dedup length check catches overlapping
    /// pages exporting a row twice, which dedup would otherwise mask.
    #[tokio::test]
    async fn export_pages_until_exhausted() {
        let repo = SqlWorkflowRepository::new(crate::storage::test_sqlite_pool().await);
        for i in 0..5 {
            let req = serde_json::from_value(serde_json::json!({
                "workflow_id": format!("wf-exp-{i}"),
                "name": format!("Export {i}"),
                "tasks": [{"id": "t1", "name": "Log",
                           "function": {"name": "log", "input": {"message": "x"}}}],
            }))
            .expect("request");
            repo.create(&req).await.expect("create");
        }

        let exported = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            collect_export_pages(&repo, &WorkflowFilter::default(), 2),
        )
        .await
        .expect("export must terminate: an endless loop means paging is broken")
        .expect("export");

        assert_eq!(
            exported.len(),
            5,
            "export must return exactly one row per workflow (no overlap, no gaps)"
        );
        let mut ids: Vec<&str> = exported.iter().map(|w| w.workflow_id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 5, "every workflow must be exported exactly once");
    }
}
