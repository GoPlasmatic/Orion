//! `/api/v2/admin/definitions` — the deployable definitions a manifest
//! member names by digest.
//!
//! Upload validates a definition as the generation builder will read it and
//! refuses whatever would not serve, then stores its canonical bytes. The
//! response's `digest` is what a member's `definition_digest` names. Upload
//! is idempotent: the same definition, however it is spelled, is one digest.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use orion_api::v2::{ChannelDefinition, Definition, DefinitionResponse, WorkflowDefinition};

use super::deployment_err_response;
use crate::errors::{FieldError, OrionError};
use crate::runtime::definitions;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

/// `POST /api/v2/admin/definitions`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/definitions",
    tag = "Deployments",
    summary = "Upload a definition a deployment manifest can name",
    description = "Validates a workflow, channel or plugin definition as the node will load it — a workflow is parsed strictly and compiled, refusing anything that would quarantine it; a channel's routing and guards are checked; a plugin's manifest is read and its component compiled and self-tested — and stores its canonical form. The answered `digest` is what a manifest member's `definition_digest` names. Idempotent: the same definition is one digest however it is spelled.",
    request_body = Definition,
    responses(
        (status = 201, description = "Definition stored", body = DefinitionResponse),
        (status = 400, description = "The definition would not serve; `details` names each problem by field"),
    )
)]
pub(crate) async fn upload_definition(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(definition): Json<Definition>,
) -> Result<impl IntoResponse, OrionError> {
    validate(&state, &definition).await?;
    let bytes = definitions::canonical_bytes(&definition)?;
    let row = match state
        .composition
        .deployments
        .store_definition_artifact_in_db(&state.db_pool, bytes)
        .await
    {
        Ok(row) => row,
        Err(e) => return Ok(deployment_err_response(e).into_response()),
    };
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_definition_upload",
        "definition",
        &row.digest,
    );
    Ok((
        StatusCode::CREATED,
        Json(DefinitionResponse {
            digest: row.digest,
            kind: definition.kind().to_string(),
            size_bytes: row.size_bytes as u64,
        }),
    )
        .into_response())
}

/// `GET /api/v2/admin/definitions/{digest}`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/definitions/{digest}",
    tag = "Deployments",
    summary = "Read a stored definition",
    params(("digest" = String, Path, description = "`sha256:…` the upload answered")),
    responses(
        (status = 200, description = "The definition as stored", body = Definition),
        (status = 404, description = "No definition is stored under that digest"),
    )
)]
pub(crate) async fn get_definition(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Json<Definition>, OrionError> {
    let bytes = state
        .composition
        .deployments
        .get_artifact(&digest)
        .and_then(|row| row.bytes)
        .ok_or_else(|| OrionError::NotFound(format!("no definition is stored as '{digest}'")))?;
    // A stored artifact that is not a definition — a connection's
    // configuration, a package file — is not one to serve here.
    let definition = definitions::parse(&bytes)
        .map_err(|_| OrionError::NotFound(format!("'{digest}' is not a definition")))?;
    Ok(Json(definition))
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/definitions", axum::routing::post(upload_definition))
        .route("/definitions/{digest}", axum::routing::get(get_definition))
}

/// Refuse a definition that would not serve, with every problem by field.
async fn validate(state: &AppState, definition: &Definition) -> Result<(), OrionError> {
    let details = match definition {
        Definition::Workflow(workflow) => workflow_problems(state, workflow)?,
        Definition::Channel(channel) => channel_problems(channel),
        Definition::Plugin(plugin) => {
            let prepared = crate::server::routes::admin::services::plugins::prepare(
                &state.config.plugins,
                None,
                &plugin.manifest,
                Some(&plugin.component),
                None,
                plugin.signature.as_deref(),
                &plugin.tags,
            )?;
            crate::server::routes::admin::services::plugins::resolve(state, prepared).await?;
            Vec::new()
        }
    };
    if details.is_empty() {
        return Ok(());
    }
    Err(OrionError::Validation {
        code: orion_api::error::codes::VALIDATION_ERROR,
        message: format!("{} definition would not serve", definition.kind()),
        details: details.into_iter().map(under_definition).collect(),
    })
}

/// A workflow is checked the way the loader reads it: the step shape, then
/// the strict parse and compile on this node's engine, refusing every issue
/// that would quarantine it.
fn workflow_problems(
    state: &AppState,
    workflow: &WorkflowDefinition,
) -> Result<Vec<FieldError>, OrionError> {
    let generation = state.runtime.load();
    let mut problems = crate::validation::validate_workflow_tasks_schema(
        &workflow.tasks,
        workflow.loop_config.as_ref(),
        &generation.functions,
    );
    if !problems.is_empty() {
        return Ok(problems);
    }
    let engine = &generation.engine;
    let row = definitions::workflow_row(workflow, "__definition__", 1)?;
    let json = crate::storage::repositories::workflows::workflow_json(
        &row,
        "__definition__",
        "__definition__",
        "active",
        None,
    )?;
    if let Err(issues) = crate::engine::compile_row(&engine.compiler(), &json) {
        problems.extend(crate::validation::quarantine_refusals(issues));
    }
    Ok(problems)
}

/// A channel is checked as a channel create is, minus its identity and its
/// workflow, which the manifest supplies.
fn channel_problems(channel: &ChannelDefinition) -> Vec<FieldError> {
    let request = crate::storage::repositories::channels::CreateChannelRequest {
        channel_id: None,
        name: channel.name.clone(),
        description: channel.description.clone(),
        channel_type: channel.channel_type,
        protocol: channel.protocol,
        methods: channel.methods.clone(),
        route_pattern: channel.route_pattern.clone(),
        topic: None,
        consumer_group: None,
        transport_config: channel.transport_config.clone(),
        workflow_id: None,
        config: channel.config.clone(),
        priority: channel.priority,
        tags: channel.tags.clone(),
    };
    match crate::validation::validate_create_channel(&request) {
        Ok(()) => Vec::new(),
        Err(e) => {
            let details = e.field_errors().to_vec();
            if details.is_empty() {
                vec![FieldError::new("", "INVALID", e.to_string())]
            } else {
                details
            }
        }
    }
}

/// A field error's path, rooted at the request's `definition`.
fn under_definition(mut error: FieldError) -> FieldError {
    error.path = if error.path.is_empty() {
        "definition".to_string()
    } else {
        format!("definition.{}", error.path)
    };
    error
}
