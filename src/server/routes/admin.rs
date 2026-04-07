use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::reload_engine;
use crate::server::state::AppState;
use crate::storage::repositories::audit_logs::AuditLogRepository;

/// Emit a structured audit log event for admin mutations.
/// Persists to the database via fire-and-forget to avoid blocking the response.
fn audit_log(
    repo: &Arc<dyn AuditLogRepository>,
    principal: Option<&AdminPrincipal>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
) {
    let who = principal
        .map(|p| p.key_prefix.as_str())
        .unwrap_or("anonymous");
    tracing::info!(
        target: "audit",
        principal = %who,
        action = %action,
        resource_type = %resource_type,
        resource_id = %resource_id,
        "admin_audit_event"
    );
    crate::metrics::record_admin_audit(action, resource_type);

    // Fire-and-forget DB persistence — audit logging must never block admin responses
    let repo = repo.clone();
    let who = who.to_string();
    let action = action.to_string();
    let resource_type = resource_type.to_string();
    let resource_id = resource_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = repo
            .insert(&who, &action, &resource_type, &resource_id, None)
            .await
        {
            tracing::warn!(error = %e, "Failed to persist audit log entry");
        }
    });
}
use crate::storage::models::{ChannelResponse, StatusAction, WorkflowResponse};
use crate::storage::repositories::channels::{
    ChannelFilter, ChannelStatusChangeRequest, CreateChannelRequest, UpdateChannelRequest,
};
use crate::storage::repositories::connectors::{
    ConnectorFilter, CreateConnectorRequest, UpdateConnectorRequest,
};
use crate::storage::repositories::workflows::{
    CreateWorkflowRequest, RolloutUpdateRequest, StatusChangeRequest, UpdateWorkflowRequest,
    WorkflowFilter,
};

pub fn admin_routes() -> Router<AppState> {
    let channel_routes = Router::new()
        .route("/", get(list_channels).post(create_channel))
        .route(
            "/{id}",
            get(get_channel).put(update_channel).delete(delete_channel),
        )
        .route("/{id}/status", patch(change_channel_status))
        .route(
            "/{id}/versions",
            get(list_channel_versions).post(create_new_channel_version),
        );

    let workflow_routes = Router::new()
        .route("/", get(list_workflows).post(create_workflow))
        .route("/import", post(import_workflows))
        .route("/export", get(export_workflows))
        .route("/validate", post(validate_workflow))
        .route(
            "/{id}",
            get(get_workflow)
                .put(update_workflow)
                .delete(delete_workflow),
        )
        .route("/{id}/status", patch(change_workflow_status))
        .route(
            "/{id}/versions",
            get(list_workflow_versions).post(create_new_workflow_version),
        )
        .route("/{id}/rollout", patch(update_rollout))
        .route("/{id}/test", post(test_workflow));

    let connector_routes = Router::new()
        .route("/", get(list_connectors).post(create_connector))
        .route(
            "/{id}",
            get(get_connector)
                .put(update_connector)
                .delete(delete_connector),
        )
        .route("/circuit-breakers", get(list_circuit_breakers))
        .route("/circuit-breakers/{key}", post(reset_circuit_breaker));

    let engine_routes = Router::new()
        .route("/status", get(engine_status))
        .route("/reload", post(engine_reload));

    let audit_routes = Router::new()
        .route("/", get(list_audit_logs));

    let mut router = Router::new()
        .nest("/channels", channel_routes)
        .nest("/workflows", workflow_routes)
        .nest("/connectors", connector_routes)
        .nest("/engine", engine_routes)
        .nest("/audit-logs", audit_routes);

    #[cfg(feature = "db-sqlite")]
    {
        let backup_routes = Router::new()
            .route("/", post(create_backup).get(list_backups));
        router = router.nest("/backups", backup_routes);
    }

    router
}

// ============================================================
// Channels CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels",
    params(ChannelFilter),
    tag = "Channels",
    responses(
        (status = 200, description = "Paginated list of channels"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_channels(
    State(state): State<AppState>,
    Query(filter): Query<ChannelFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.channel_repo.list_paginated(&filter).await?;
    let data: Vec<ChannelResponse> = result
        .data
        .iter()
        .map(ChannelResponse::try_from)
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
    path = "/api/v1/admin/channels",
    tag = "Channels",
    request_body = CreateChannelRequest,
    responses(
        (status = 201, description = "Channel created as draft"),
        (status = 400, description = "Invalid input"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_channel(&req)?;
    let channel = state.channel_repo.create(&req).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create",
        "channel",
        &channel.channel_id,
    );
    // No engine reload — drafts are not in the engine
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": ChannelResponse::try_from(&channel)? })),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 200, description = "Channel details"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let channel = state.channel_repo.get_by_id(&id).await?;
    Ok(Json(json!({
        "data": ChannelResponse::try_from(&channel)?,
    })))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    request_body = UpdateChannelRequest,
    responses(
        (status = 200, description = "Draft channel updated"),
        (status = 400, description = "No draft version or invalid input"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateChannelRequest>,
) -> Result<Json<Value>, OrionError> {
    let channel = state.channel_repo.update_draft(&id, &req).await?;
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "update", "channel", &id);
    // No engine reload — drafts are not in the engine
    Ok(Json(
        json!({ "data": ChannelResponse::try_from(&channel)? }),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/channels/{id}",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 204, description = "Channel deleted"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.channel_repo.delete(&id).await?;
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "delete", "channel", &id);
    reload_engine(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Channel Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/channels/{id}/status",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    request_body = ChannelStatusChangeRequest,
    responses(
        (status = 200, description = "Status updated"),
        (status = 400, description = "Invalid status transition"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_channel_status(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<ChannelStatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status.as_str())?;
    let channel = match action {
        StatusAction::Activate => state.channel_repo.activate(&id).await?,
        StatusAction::Archive => state.channel_repo.archive(&id).await?,
    };
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        &format!("status_{}", req.status),
        "channel",
        &id,
    );
    reload_engine(&state).await?;
    Ok(Json(
        json!({ "data": ChannelResponse::try_from(&channel)? }),
    ))
}

// ============================================================
// Channel Version Management
// ============================================================

#[derive(Debug, Deserialize)]
pub(crate) struct VersionFilter {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/channels/{id}/versions",
    tag = "Channels",
    params(
        ("id" = String, Path, description = "Channel ID"),
    ),
    responses(
        (status = 200, description = "Paginated version history"),
        (status = 404, description = "Channel not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_channel_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(filter): Query<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    // Verify channel exists
    let _ = state.channel_repo.get_by_id(&id).await?;

    let limit = filter.limit.unwrap_or(50);
    let offset = filter.offset.unwrap_or(0);
    let result = state.channel_repo.list_versions(&id, limit, offset).await?;
    let data: Vec<ChannelResponse> = result
        .data
        .iter()
        .map(ChannelResponse::try_from)
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
    path = "/api/v1/admin/channels/{id}/versions",
    tag = "Channels",
    params(("id" = String, Path, description = "Channel ID")),
    responses(
        (status = 201, description = "New draft version created"),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_new_channel_version(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let channel = state.channel_repo.create_new_version(&id).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create_version",
        "channel",
        &id,
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": ChannelResponse::try_from(&channel)? })),
    ))
}

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
    Ok(Json(json!({
        "data": data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
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
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": WorkflowResponse::try_from(&workflow)? })),
    ))
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
    Ok(Json(json!({
        "data": WorkflowResponse::try_from(&workflow)?,
    })))
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
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "update", "workflow", &id);
    // No engine reload — drafts are not in the engine
    Ok(Json(
        json!({ "data": WorkflowResponse::try_from(&workflow)? }),
    ))
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
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "delete", "workflow", &id);
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
    Ok(Json(
        json!({ "data": WorkflowResponse::try_from(&workflow)? }),
    ))
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
    Ok(Json(
        json!({ "data": WorkflowResponse::try_from(&workflow)? }),
    ))
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

    Ok(Json(json!({
        "data": data,
        "total": result.total,
        "limit": result.limit,
        "offset": result.offset,
    })))
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
    Ok((
        StatusCode::CREATED,
        Json(json!({ "data": WorkflowResponse::try_from(&workflow)? })),
    ))
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
    Ok(Json(json!({ "data": data })))
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
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<CreateConnectorRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_connector(&req)?;
    let connector = state.connector_repo.create(&req).await?;
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create",
        "connector",
        &connector.id,
    );
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
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConnectorRequest>,
) -> Result<Json<Value>, OrionError> {
    crate::validation::validate_update_connector(&req)?;
    let connector = state.connector_repo.update(&id, &req).await?;
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "update", "connector", &id);
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
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    state.connector_repo.delete(&id).await?;
    audit_log(&state.audit_log_repo, principal.as_ref().map(|e| &e.0), "delete", "connector", &id);
    reload_connectors(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Circuit Breakers
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/circuit-breakers",
    tag = "Connectors",
    responses(
        (status = 200, description = "Circuit breaker states"),
    )
)]
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

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/circuit-breakers/{key}",
    tag = "Connectors",
    params(("key" = String, Path, description = "Circuit breaker key (channel:connector)")),
    responses(
        (status = 200, description = "Circuit breaker reset"),
        (status = 404, description = "Circuit breaker not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn reset_circuit_breaker(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let found = state.connector_registry.reset_circuit_breaker(&key).await;
    if found {
        audit_log(
            &state.audit_log_repo,
            principal.as_ref().map(|e| &e.0),
            "reset",
            "circuit_breaker",
            &key,
        );
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
    let engine = crate::engine::acquire_engine_read(&state.engine).await;
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
        "workflows_count": workflows.len(),
        "active_workflows": active_count,
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
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn engine_reload(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "reload",
        "engine",
        "manual",
    );
    reload_engine(&state).await?;

    let engine = crate::engine::acquire_engine_read(&state.engine).await;
    let workflows_count = engine.workflows().len();

    Ok(Json(json!({
        "reloaded": true,
        "workflows_count": workflows_count,
    })))
}

// ============================================================
// Backups (SQLite only)
// ============================================================

#[cfg(feature = "db-sqlite")]
async fn create_backup(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    let backup_dir = &state.config.storage.backup_dir;

    std::fs::create_dir_all(backup_dir).map_err(|e| OrionError::InternalSource {
        context: format!("Failed to create backup directory '{backup_dir}'"),
        source: Box::new(e),
    })?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("orion_backup_{timestamp}.db");
    let backup_path = std::path::Path::new(backup_dir).join(&filename);
    let backup_path_str = backup_path.to_string_lossy().to_string();

    sqlx::query(&format!("VACUUM INTO '{}'", backup_path_str.replace('\'', "''")))
        .execute(&state.db_pool)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to create database backup".to_string(),
            source: Box::new(e),
        })?;

    let metadata = std::fs::metadata(&backup_path).map_err(|e| OrionError::InternalSource {
        context: "Failed to read backup file metadata".to_string(),
        source: Box::new(e),
    })?;

    audit_log(
        &state.audit_log_repo,
        principal.as_ref().map(|e| &e.0),
        "create",
        "backup",
        &filename,
    );

    Ok(Json(json!({
        "data": {
            "filename": filename,
            "path": backup_path_str,
            "size_bytes": metadata.len(),
            "created_at": chrono::Utc::now().to_rfc3339(),
        }
    })))
}

#[cfg(feature = "db-sqlite")]
async fn list_backups(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let backup_dir = &state.config.storage.backup_dir;

    let dir = match std::fs::read_dir(backup_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Json(json!({ "data": [] })));
        }
        Err(e) => {
            return Err(OrionError::InternalSource {
                context: format!("Failed to read backup directory '{backup_dir}'"),
                source: Box::new(e),
            });
        }
    };

    let mut backups = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "db")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("orion_backup_"))
            && let Ok(meta) = entry.metadata()
        {
            let modified = meta
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();
            backups.push(json!({
                "filename": path.file_name().unwrap_or_default().to_string_lossy(),
                "size_bytes": meta.len(),
                "modified_at": modified,
            }));
        }
    }

    // Sort by filename (which includes timestamp) descending
    backups.sort_by(|a, b| {
        b["filename"]
            .as_str()
            .cmp(&a["filename"].as_str())
    });

    Ok(Json(json!({ "data": backups })))
}

// ============================================================
// Audit Logs
// ============================================================

#[derive(Debug, Deserialize)]
struct AuditLogQuery {
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_audit_logs(
    State(state): State<AppState>,
    Query(params): Query<AuditLogQuery>,
) -> Result<Json<Value>, OrionError> {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(50).clamp(1, 1000);

    let entries = state.audit_log_repo.list_paginated(offset, limit).await?;
    let total = state.audit_log_repo.count().await?;

    Ok(Json(json!({
        "data": entries,
        "pagination": {
            "offset": offset,
            "limit": limit,
            "total": total,
        }
    })))
}
