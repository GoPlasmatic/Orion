use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use dataflow_rs::datalogic_rs;
use serde::Deserialize;
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
use super::audit_and_reload;
use super::audit_log;
use super::audit_log_draft_only;
use super::{ValidationEnvelope, ValidationIssue, issues_from_error};
use crate::storage::repositories::helpers::VersionFilter;

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
    let result = state.repos.workflows.list_paginated(&filter).await?;
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
    let workflow = state.repos.workflows.create(&req).await?;
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
    let workflow = state.repos.workflows.get_by_id(&id).await?;
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
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Workflow not found, or it has no draft version to update"),
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
    let workflow = state.repos.workflows.update_draft(&id, &req).await?;
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
    state.repos.workflows.delete(&id).await?;
    audit_and_reload(
        &state,
        &principal,
        "delete",
        "workflow",
        &id,
        super::ReloadMode::Now,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Workflow Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/workflows/{id}/status",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID"), super::StatusChangeQuery),
    request_body = StatusChangeRequest,
    responses(
        (status = 200, description = "Status updated. With `?dry_run=true` nothing is \
            written and the body is instead the `/validate` envelope \
            (`{\"data\": {\"valid\", \"errors\", \"warnings\"}}`) reporting every gate \
            the real transition would run: draft existence, connector existence and \
            type match, MongoDB `database` presence, and rollout arithmetic (K3). \
            With `?reload=defer` the row commits but the engine (and every cluster \
            peer) keeps serving the previous active set until \
            `POST /engine/reload` (K4).", body = DataEnvelope<WorkflowResponse>),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_workflow_status(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::StatusChangeQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status)?;
    if query.dry_run {
        let envelope = dry_run_status_change(&state, &id, &action, &req).await?;
        return Ok(Json(serde_json::to_value(envelope)?));
    }
    let workflow = match action {
        StatusAction::Activate => {
            // R5: refuse to activate a workflow that cannot run. Connector
            // references stay a warning at create time (connectors and
            // workflows may be authored in either order) — activation is
            // the gate, because from here the workflow serves traffic and
            // a missing connector is a guaranteed runtime 500.
            let draft = state.repos.workflows.get_by_id(&id).await?;
            ensure_workflow_connectors_exist(&state, &draft).await?;
            let rollout_pct = req.rollout_percentage.unwrap_or(100);
            state.repos.workflows.activate(&id, rollout_pct).await?
        }
        StatusAction::Archive => state.repos.workflows.archive(&id).await?,
    };
    audit_and_reload(
        &state,
        &principal,
        &format!("status_{}", req.status),
        "workflow",
        &id,
        query.reload,
    )
    .await?;
    Ok(data_response(WorkflowResponse::try_from(&workflow)?))
}

/// K3: every gate the real transition runs, as findings instead of failures.
///
/// The rule that keeps this honest is the `/validate` rule (R20) one endpoint
/// over: the checks are the *same functions* the real path calls
/// ([`ensure_workflow_connectors_exist`], the repository's rollout bounds),
/// so `valid: true` cannot come to mean something weaker than "the
/// un-dry-run request would succeed". Findings that the real path reports as
/// a 4xx arrive here as `errors` entries in a 200 — a plan wants the report,
/// not the failure — including "not found", so a CLI can pre-flight a whole
/// package without tripping over the first missing entity.
async fn dry_run_status_change(
    state: &AppState,
    id: &str,
    action: &StatusAction,
    req: &StatusChangeRequest,
) -> Result<ValidationEnvelope, OrionError> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    match action {
        StatusAction::Activate => {
            let rollout_pct = req.rollout_percentage.unwrap_or(100);
            if !(0..=100).contains(&rollout_pct) {
                errors.push(ValidationIssue {
                    field: "rollout_percentage".to_string(),
                    message: "rollout_percentage must be between 0 and 100".to_string(),
                });
            }
            match state.repos.workflows.get_by_id(id).await {
                Ok(latest) => {
                    if latest.status != crate::storage::models::EntityStatus::Draft.as_str() {
                        errors.push(ValidationIssue {
                            field: "status".to_string(),
                            message: format!(
                                "No draft version found for workflow '{id}' — create a new \
                                 version first"
                            ),
                        });
                    } else if let Err(e) = ensure_workflow_connectors_exist(state, &latest).await {
                        errors.extend(issues_from_error(e));
                    }
                    // A partial rollout with nothing to share traffic with is
                    // accepted by the real path and quarantined at engine
                    // load — say so here, where it is still cheap to fix.
                    if (0..100).contains(&rollout_pct) {
                        let has_active = state
                            .repos
                            .workflows
                            .list_active()
                            .await?
                            .iter()
                            .any(|w| w.workflow_id == id);
                        if !has_active {
                            warnings.push(ValidationIssue {
                                field: "rollout_percentage".to_string(),
                                message: format!(
                                    "partial rollout of {rollout_pct}% with no currently \
                                     active version: the active set would sum to \
                                     {rollout_pct}%, and the serving channel is quarantined \
                                     until rollout percentages sum to 100"
                                ),
                            });
                        }
                    }
                }
                Err(OrionError::NotFound(_)) => errors.push(ValidationIssue {
                    field: "(root)".to_string(),
                    message: format!("Workflow '{id}' not found"),
                }),
                Err(e) => return Err(e),
            }
        }
        StatusAction::Archive => {
            let has_active = state
                .repos
                .workflows
                .list_active()
                .await?
                .iter()
                .any(|w| w.workflow_id == id);
            if !has_active {
                errors.push(ValidationIssue {
                    field: "status".to_string(),
                    message: format!("No active version found for workflow '{id}'"),
                });
            }
        }
    }

    Ok(ValidationEnvelope::new(errors, warnings))
}

// The task-reference walk lives in `crate::engine::refs` so the K9 endpoint,
// the gates here, the rename guard in `admin/connectors.rs`, and the package
// CLI's offline lint all share one implementation.
pub(crate) use crate::engine::refs::{channel_call_targets, connector_refs};

/// `GET /api/v1/admin/workflows/{id}/dependencies` (K9).
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct WorkflowDependencies {
    workflow_id: String,
    /// The version whose tasks were walked (the latest).
    version: i64,
    /// Connector names the tasks reference, with the referencing function —
    /// duplicates collapsed, task order kept.
    connectors: Vec<ConnectorDependency>,
    /// Channel names targeted by `channel_call` tasks, statically.
    channels: Vec<String>,
    /// True when a `channel_call` resolves its target with `channel_logic`
    /// at runtime — the static `channels` list is then incomplete by
    /// construction, and closure tooling must treat this workflow as having
    /// unknowable channel dependencies.
    has_dynamic_channel_calls: bool,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ConnectorDependency {
    connector: String,
    /// The task function that uses it (`db_read`, `http_call`, …).
    function: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/workflows/{id}/dependencies",
    tag = "Workflows",
    params(("id" = String, Path, description = "Workflow ID")),
    responses(
        (status = 200, description = "What the workflow's tasks reference (K9): connector \
            names (with the referencing function) and statically-known `channel_call` \
            targets. The API twin of the reference walk activation runs, for tooling \
            that computes a package's dependency closure without re-implementing the \
            task-walk.", body = DataEnvelope<WorkflowDependencies>),
        (status = 404, description = "Workflow not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn workflow_dependencies(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let workflow = state.repos.workflows.get_by_id(&id).await?;
    let tasks: Value = serde_json::from_str(&workflow.tasks_json).map_err(|e| {
        OrionError::internal(format!("Corrupt JSON in workflow {id} tasks_json: {e}"))
    })?;

    let mut connectors: Vec<ConnectorDependency> = Vec::new();
    for r in connector_refs(&tasks) {
        if !connectors
            .iter()
            .any(|c| c.connector == r.connector && c.function == r.function)
        {
            connectors.push(ConnectorDependency {
                connector: r.connector.to_string(),
                function: r.function.to_string(),
            });
        }
    }
    let (channels, has_dynamic_channel_calls) = channel_call_targets(&tasks);

    Ok(data_response(WorkflowDependencies {
        workflow_id: workflow.workflow_id,
        version: workflow.version,
        connectors,
        channels: channels.into_iter().map(str::to_string).collect(),
        has_dynamic_channel_calls,
    }))
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
    params(("id" = String, Path, description = "Workflow ID"), super::ReloadQuery),
    request_body = RolloutUpdateRequest,
    responses(
        (status = 200, description = "Rollout percentage updated. With `?reload=defer` \
            the row commits but the engine keeps serving the previous rollout until \
            `POST /engine/reload` (K4).", body = DataEnvelope<WorkflowResponse>),
        (status = 400, description = "Invalid rollout configuration"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_rollout(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ReloadQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<RolloutUpdateRequest>,
) -> Result<Json<Value>, OrionError> {
    let workflow = state
        .repos
        .workflows
        .update_rollout(&id, req.rollout_percentage)
        .await?;
    audit_and_reload(
        &state,
        &principal,
        "update_rollout",
        "workflow",
        &id,
        query.reload,
    )
    .await?;
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
        VersionFilter,
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
    let _ = state.repos.workflows.get_by_id(&id).await?;

    let result = state.repos.workflows.list_versions(&id, &filter).await?;
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
    let workflow = state.repos.workflows.create_new_version(&id).await?;
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

    let workflow = state.repos.workflows.get_by_id(&id).await?;

    // O7: this endpoint runs the workflow's tasks against **live connectors** —
    // it will POST to real webhooks, write to real databases and publish to
    // real topics. It read as a harmless dry run and emitted no audit event at
    // all, so the most side-effecting call on the admin plane was the one
    // operation the trail could not show. Recorded before execution, so the
    // attempt is on the record even if the run itself fails or the process
    // dies mid-task.
    audit_log(&state.audit_queue, &principal, "test", "workflow", &id);

    // G14 keeps `OrionError::Serialization` a 500, on the premise that no
    // client-authored parse reaches it via `?` — so this conversion of a
    // client-authored draft must answer for itself. A stored workflow the
    // engine's shape refuses (a gap the create-time task validation did not
    // cover) is the author's document to fix, not a server fault: 400 with
    // the shape error, which quotes their own workflow JSON and nothing else.
    let df_workflow = workflow_to_dataflow(&workflow, "__test__").map_err(|e| {
        OrionError::validation(format!(
            "workflow '{id}' does not match the engine's workflow shape: {e}"
        ))
    })?;

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

    let mut message = dataflow_rs::Message::builder()
        .payload_json(&payload)
        .metadata_json(&req.metadata)
        .build();

    // Record into a trace this function owns, so the steps that ran before a
    // hard failure survive it. `process_message_with_trace` builds the trace as
    // a local and moves it into the `Ok` arm, so an `Err` used to leave this
    // endpoint — the one place a human explicitly asks "show me the steps" —
    // returning a bare 5xx with no steps at all. A failed dry run is the case
    // it exists to explain, so it answers 200 with the partial trace and the
    // error rather than throwing both away.
    let mut trace = dataflow_rs::ExecutionTrace::new();
    let run_error = test_engine
        .process_message_tracing(&mut message, &mut trace)
        .await
        .err();

    let matched = !trace.steps.is_empty()
        && trace.steps.iter().any(|s| {
            matches!(
                s.result,
                dataflow_rs::StepResult::Executed | dataflow_rs::StepResult::Skipped
            )
        });

    let trace_value = serde_json::to_value(&trace)?;
    let mut body = json!({
        "matched": matched,
        "trace": trace_value,
        "output": message.data(),
        "errors": message.errors().iter().filter_map(|e| serde_json::to_value(e).ok()).collect::<Vec<_>>(),
    });
    if let Some(e) = run_error {
        // Note the failing task's own step is still absent: the engine
        // propagates before appending it, so the trace ends at the last
        // known-good step and this names what stopped it.
        //
        // G12: through the same redaction as every other engine-error
        // surface — the raw Display can carry driver text. The full error
        // goes to the log; validation/timeout messages stay verbatim.
        tracing::warn!(error = %e, "workflow dry-run execution failed");
        body["error"] = json!(OrionError::Engine(e).client_message());
    }

    Ok(data_response(body))
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
            conflicts against stored rows and duplicates within the batch, without writing. \
            `?on_conflict=new_version` upserts instead of refusing an existing id (K2): an existing \
            draft is replaced, an active workflow whose content differs gets a new draft version, \
            and identical content is reported `unchanged` — re-importing the same artifact is a \
            no-op. Per-item outcomes are in `results`.", body = DataEnvelope<ImportResult>),
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
    let repo = state.repos.workflows.clone();
    let probe = state.repos.workflows.clone();
    let upsert_repo = state.repos.workflows.clone();
    let outcome = super::import_items::<CreateWorkflowRequest, _, _, _, _, _, _, _, _>(
        items,
        query.dry_run,
        query.on_conflict,
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
            upsert: |w: CreateWorkflowRequest, dry_run: bool| {
                let repo = upsert_repo.clone();
                async move { upsert_workflow(repo.as_ref(), w, dry_run).await }
            },
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }

    // K5: one row per written entity — the trail used to record only
    // "{n} imported", which could not answer *what* an import created — plus
    // the summary row that ties the batch together.
    for id in outcome.written() {
        audit_log_draft_only(&state.audit_queue, &principal, "import", "workflow", id);
    }
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "workflow",
        &format!("{} imported", outcome.imported),
    );

    Ok(super::import_response(false, outcome))
}

/// K2: one workflow item under `on_conflict=new_version`. The decision table
/// (including the archived-entity invariant) is [`super::versioned_upsert_action`];
/// this supplies the workflow repository's verbs.
async fn upsert_workflow(
    repo: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    req: CreateWorkflowRequest,
    dry_run: bool,
) -> Result<super::ImportAction, OrionError> {
    use super::ImportAction;

    let Some(id) = req.workflow_id.clone() else {
        // No id → the store generates one; nothing to conflict with.
        if !dry_run {
            repo.create(&req).await?;
        }
        return Ok(ImportAction::Created);
    };
    let latest = match repo.get_by_id(&id).await {
        Ok(latest) => latest,
        Err(OrionError::NotFound(_)) => {
            if !dry_run {
                repo.create(&req).await?;
            }
            return Ok(ImportAction::Created);
        }
        Err(e) => return Err(e),
    };

    let identical = crate::storage::content::workflow_content(&latest)?
        == crate::storage::content::workflow_request_content(&req);
    let action = super::versioned_upsert_action(&latest.status, identical);
    if !dry_run {
        match action {
            ImportAction::UpdatedDraft => {
                repo.replace_draft(&id, &req).await?;
            }
            ImportAction::NewVersion => {
                repo.create_new_version(&id).await?;
                repo.replace_draft(&id, &req).await?;
            }
            _ => {}
        }
    }
    Ok(action)
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
    // K12: one repeatable-read transaction — the export is a consistent
    // snapshot, not a sequence of independent page queries.
    let rows = state.repos.workflows.snapshot(&filter).await?;
    let data: Vec<WorkflowResponse> = rows
        .iter()
        .map(WorkflowResponse::try_from)
        .collect::<Result<_, _>>()?;
    Ok(data_response(data))
}

// ============================================================
// Workflow Validation
// ============================================================

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
    Ok(Json(run_validation(&req, &state).await))
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
async fn run_validation(req: &CreateWorkflowRequest, state: &AppState) -> ValidationEnvelope {
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

    ValidationEnvelope::new(errors, warnings)
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
/// to [`errors_for_task`] and tracking cross-task state — duplicate IDs, and
/// the running set of context paths earlier tasks have written — here.
async fn validate_tasks(
    tasks: &[Value],
    dl: &datalogic_rs::Engine,
    state: &AppState,
    errors: &mut Vec<ValidationIssue>,
    warnings: &mut Vec<ValidationIssue>,
) {
    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut written: Vec<String> = Vec::new();

    for (i, task) in tasks.iter().enumerate() {
        let (task_errors, task_warnings) = errors_for_task(i, task, dl, state).await;
        errors.extend(task_errors);
        warnings.extend(task_warnings);

        warn_on_unwritten_reads(i, task, &mut written, warnings);

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

/// Most warnings any one workflow reports about unwritten reads.
///
/// A workflow whose first task is missing produces one of these per read in
/// every later task; past a handful the list stops informing and starts burying
/// the errors above it.
const MAX_UNWRITTEN_READ_WARNINGS: usize = 10;

/// Warn about `data.*` paths this task reads that nothing has written yet.
///
/// A mistyped path is the highest-frequency authoring bug and the least visible
/// one: JSONLogic resolves an unknown `var` to null, so the task runs, the
/// workflow succeeds, and the caller gets a `200` with a field quietly missing.
/// Nothing else in the pipeline is in a position to notice.
///
/// **A warning, never an error**, and deliberately so. The reader could be
/// legitimate in ways this walk cannot see — a `continue_on_error` predecessor
/// whose write did not happen, a path built by a connector response shape, or
/// simply a workflow that is correct and a heuristic that is not. `valid: true`
/// has to keep meaning "create would accept this" (R20), so a false positive
/// here must cost the author a glance and never a refusal.
///
/// Writes are accumulated across the walk and matched by path prefix in both
/// directions: writing `data.order` covers a read of `data.order.total`
/// (reading into the object), and writing `data.order.total` covers a read of
/// `data.order` (reading the object it lives in).
fn warn_on_unwritten_reads(
    i: usize,
    task: &Value,
    written: &mut Vec<String>,
    warnings: &mut Vec<ValidationIssue>,
) {
    let mut report = |path: &str, field: String| {
        if warnings.len() >= MAX_UNWRITTEN_READ_WARNINGS
            || warnings
                .iter()
                .any(|w| w.field == field && w.message.contains(path))
        {
            return;
        }
        warnings.push(ValidationIssue {
            field,
            message: format!(
                "reads '{path}', which no earlier task writes. If this is a typo the \
                 task will silently see null; if the value arrives another way \
                 (metadata, a connector response shape, continue_on_error), ignore this."
            ),
        });
    };

    // The task's own condition is evaluated before any of its writes land.
    if let Some(condition) = task.get("condition") {
        for path in data_reads(condition) {
            if !is_written(&path, written) {
                report(&path, format!("tasks[{i}].condition"));
            }
        }
    }

    // `map` applies its mappings in order and a later one may legitimately read
    // an earlier one's target, so its writes land as the walk passes them
    // rather than all at the end.
    let mappings = task
        .get("function")
        .and_then(|f| f.get("input"))
        .and_then(|input| input.get("mappings"))
        .and_then(|m| m.as_array());

    if let Some(mappings) = mappings {
        for (m, mapping) in mappings.iter().enumerate() {
            if let Some(logic) = mapping.get("logic") {
                for path in data_reads(logic) {
                    if !is_written(&path, written) {
                        report(
                            &path,
                            format!("tasks[{i}].function.input.mappings[{m}].logic"),
                        );
                    }
                }
            }
            if let Some(path) = mapping.get("path").and_then(|p| p.as_str()) {
                written.push(path.to_string());
            }
        }
        return;
    }

    if let Some(input) = task.get("function").and_then(|f| f.get("input")) {
        for path in data_reads(input) {
            if !is_written(&path, written) {
                report(&path, format!("tasks[{i}].function.input"));
            }
        }
    }
    written.extend(task_writes(task));
}

/// Whether `path` is covered by something already written, by prefix in either
/// direction. A bare `data` write is the whole context and covers everything.
fn is_written(path: &str, written: &[String]) -> bool {
    written.iter().any(|w| {
        w == "data"
            || w == path
            || path.starts_with(&format!("{w}."))
            || w.starts_with(&format!("{path}."))
    })
}

/// Context paths a task writes, for every function that writes one.
///
/// `parse_json`/`parse_xml`/`publish_json`/`publish_xml` take a bare `target`
/// under `data`; the connector functions take a full dotted `output` path
/// (`response_path` is the accepted pre-1.0 spelling — see `output_field_test`).
/// `data_query`/`data_write` default that output to the `data` root when it is
/// omitted, which is why the default is spelled out rather than skipped.
fn task_writes(task: &Value) -> Vec<String> {
    let Some(function) = task.get("function") else {
        return Vec::new();
    };
    let name = function.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let Some(input) = function.get("input") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    match name {
        "parse_json" | "parse_xml" | "publish_json" | "publish_xml" => {
            if let Some(target) = input.get("target").and_then(|t| t.as_str()) {
                out.push(format!("data.{target}"));
            }
        }
        _ => {
            match input
                .get("output")
                .or_else(|| input.get("response_path"))
                .and_then(|o| o.as_str())
            {
                Some(path) => out.push(path.to_string()),
                None if matches!(name, "data_query" | "data_write") => out.push("data".to_string()),
                None => {}
            }
        }
    }
    out
}

/// Every `data.*` path a JSON subtree reads through `var`/`val`.
///
/// Only `data.`-rooted reads are collected. `metadata.*`, `payload`, the
/// element rebinding inside `map`/`filter`/`reduce` (`{"var": "price"}`,
/// `accumulator`, `current`) and the empty path are all legitimate reads of
/// something this walk does not track, and warning about them would be noise.
fn data_reads(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_data_reads(value, &mut out);
    out
}

fn collect_data_reads(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "var" | "val") {
                    // `{"var": "data.x"}` and `{"var": ["data.x", default]}`
                    // are both spellings of one read.
                    let path = match child {
                        Value::String(s) => Some(s.as_str()),
                        Value::Array(items) => items.first().and_then(|f| f.as_str()),
                        _ => None,
                    };
                    if let Some(path) = path
                        && (path == "data" || path.starts_with("data."))
                    {
                        out.push(path.to_string());
                    }
                }
                collect_data_reads(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_data_reads(item, out);
            }
        }
        _ => {}
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
    use crate::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

    /// D7/K12 regression: the export snapshot must page in bounded queries
    /// and still return every matching workflow exactly once. With a page
    /// size of 2 and 5 rows the loop runs three times inside one
    /// transaction; if it stops after the first page, or the select ignores
    /// its limit, an assertion fails. The `timeout` matters: a select that
    /// ignores its limit never returns a short page, so an unbounded call
    /// would spin forever — the revert must show up as a red test, not a
    /// hang. The pre-dedup length check catches overlapping pages exporting
    /// a row twice, which dedup would otherwise mask.
    #[tokio::test]
    async fn export_snapshot_pages_until_exhausted() {
        use crate::storage::schema::{CurrentWorkflows, Workflows};
        use sea_query::{Asterisk, Order, Query};

        let pool = crate::storage::test_sqlite_pool().await;
        let repo = SqlWorkflowRepository::new(pool.clone());
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

        // The same page shape `snapshot` builds, at a page size small enough
        // to prove the loop (the repo method's 500 would finish in one page).
        let exported: Vec<crate::storage::models::Workflow> = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            crate::storage::repositories::helpers::snapshot_pages(&pool, 2, |limit, offset| {
                Query::select()
                    .column(Asterisk)
                    .from(CurrentWorkflows::Table)
                    .order_by(Workflows::WorkflowId, Order::Asc)
                    .limit(limit as u64)
                    .offset(offset as u64)
                    .to_owned()
            }),
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
