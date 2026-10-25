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
use orion_api::v2::{
    ChannelDefinition, Definition, DefinitionResponse, TestDefinitionRequest, WorkflowDefinition,
};
use serde_json::{Value, json};

use super::deployment_err_response;
use crate::errors::{FieldError, OrionError};
use crate::runtime::definitions;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionJson;
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
    OrionJson(mut definition): OrionJson<Definition>,
) -> Result<impl IntoResponse, OrionError> {
    validate(&state, &mut definition).await?;
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

/// The channel a tested workflow is compiled and run on.
const TEST_CHANNEL: &str = "__test__";

/// `POST /api/v2/admin/definitions/test`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/definitions/test",
    tag = "Deployments",
    summary = "Run a workflow definition once, without deploying it",
    description = "Refuses a workflow definition `POST /definitions` would refuse, then runs it once on this node's engine — its functions, plugins and connectors, so tasks reach **live backends** — against the given `data` and `metadata`, normalised the way an ingress builds them with this node's `[vars]` stamped in. Nothing is stored and nothing is stubbed. The trace is kept on every outcome, so a run that stopped short still answers `200` with the steps that ran and the `error` that stopped it. Recorded in the audit log before the run.",
    request_body = TestDefinitionRequest,
    responses(
        (status = 200, description = "The run, with its trace", body = crate::server::routes::openapi::schemas::WorkflowTestResult),
        (status = 400, description = "The definition would not serve, or `metadata` is not an object; `details` names each problem by field"),
    )
)]
pub(crate) async fn test_definition(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(request): OrionJson<TestDefinitionRequest>,
) -> Result<Json<Value>, OrionError> {
    let TestDefinitionRequest {
        definition,
        data,
        metadata,
    } = request;
    let problems = workflow_problems(&state, &definition)?;
    if !problems.is_empty() {
        return Err(OrionError::Validation {
            code: orion_api::error::codes::VALIDATION_ERROR,
            message: "workflow definition would not serve".to_string(),
            details: problems.into_iter().map(under_definition).collect(),
        });
    }
    let metadata = crate::engine::utils::prepare_offline_metadata(metadata).map_err(|e| {
        OrionError::Validation {
            code: orion_api::error::codes::VALIDATION_ERROR,
            message: e.clone(),
            details: vec![FieldError::new("metadata", "INVALID", e)],
        }
    })?;

    // The run reaches live connectors — it will POST to real webhooks and
    // write to real databases — so it is on the record before it starts,
    // even if the run fails or the process dies mid-task.
    let digest = crate::crypto::sha256_digest(&definitions::canonical_bytes(
        &Definition::Workflow(definition.clone()),
    )?);
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_definition_test",
        "definition",
        &digest,
    );

    // An isolated engine over just this workflow, compiled on the serving
    // generation's own registry — its functions, plugins and connectors — as
    // a deployment compiles a member.
    let generation = state.runtime.load();
    let row = definitions::workflow_row(&definition, TEST_CHANNEL, 1)?;
    let workflow_json =
        crate::engine::workflow_json(&row, TEST_CHANNEL, TEST_CHANNEL, "active", None)?;
    let engine =
        crate::engine::offline::single_engine(&generation.engine.compiler(), &workflow_json)?;

    // Through the same normalisation the offline surfaces use, so a run here
    // and a real request agree; `vars` is the one key stamped rather than
    // accepted, because this node has real values for it.
    let mut metadata = metadata;
    crate::engine::stamp_vars(&mut metadata, state.vars.as_deref());

    // Bounded as a node bounds a `task_details` trace: past the budget a step
    // keeps its id, result and timing and drops its snapshot.
    let result = crate::engine::offline::run_single(
        &engine,
        &data,
        &metadata,
        crate::engine::offline::OfflineRun {
            trace: Some(crate::engine::offline::OfflineTrace::Changes),
            max_snapshot_bytes: state.config.trace_queue.max_result_size_bytes,
            capture_changes: true,
            layers: &[],
            observer: None,
        },
    )
    .await;

    let message = &result.message;
    let mut body = json!({
        "matched": result.matched,
        "trace": serde_json::to_value(&result.trace)?,
        "output": Value::from(message.context().data()),
        "errors": serde_json::to_value(message.errors())?,
    });
    if let Some(e) = result.error {
        // Through the same redaction as every other engine-error surface:
        // the raw text can carry driver detail, so it goes to the log.
        tracing::warn!(error = %e, "definition test run failed");
        body["error"] = json!(OrionError::Engine(e).client_message());
    }
    Ok(Json(body))
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/definitions", axum::routing::post(upload_definition))
        .route("/definitions/test", axum::routing::post(test_definition))
        .route("/definitions/{digest}", axum::routing::get(get_definition))
}

/// Refuse a definition that would not serve, with every problem by field.
/// A plugin's manifest is stored as the parsed JSON the loader reads, however
/// it was written (`plugin.toml` text or an object).
async fn validate(state: &AppState, definition: &mut Definition) -> Result<(), OrionError> {
    let details = match definition {
        Definition::Workflow(workflow) => workflow_problems(state, workflow)?,
        Definition::Channel(channel) => channel_problems(channel),
        Definition::Plugin(plugin) => {
            plugin.manifest = super::plugin_definition::check(
                state,
                &plugin.manifest,
                &plugin.component,
                plugin.signature.as_deref(),
            )
            .await?;
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

/// A workflow is checked the way the loader reads it: everything a definition
/// set's workflow is checked for — its name, uncompiled source form, the loop,
/// every step's function input, secret references where nothing resolves one
/// — then the strict parse and compile on this node's engine, refusing every
/// issue that would quarantine it.
fn workflow_problems(
    state: &AppState,
    workflow: &WorkflowDefinition,
) -> Result<Vec<FieldError>, OrionError> {
    let generation = state.runtime.load();
    // A qualified name (`acme.iso8583.parse`) the node does not serve names a
    // plugin function, whose plugin may arrive in the same deployment. Upload
    // cannot know which plugins that is; the deployment's build check
    // compiles the workflow against the plugins the manifest carries, so a
    // name it cannot resolve is refused there.
    let deferred: std::collections::BTreeSet<&str> =
        crate::engine::leaf_tasks(&workflow.tasks, workflow.loop_config.as_ref())
            .into_iter()
            .filter_map(|task| task.pointer("/function/name").and_then(Value::as_str))
            .filter(|name| name.contains('.') && !generation.functions.contains(name))
            .collect();
    let is_deferred = |e: &FieldError| {
        e.code == "UNKNOWN_FUNCTION"
            && deferred
                .iter()
                .any(|name| e.message.contains(&format!("'{name}'")))
    };
    let request = crate::storage::models::CreateWorkflowRequest {
        workflow_id: None,
        name: workflow.name.clone(),
        description: workflow.description.clone(),
        priority: workflow.priority,
        condition: workflow.condition.clone(),
        tasks: workflow.tasks.clone(),
        tags: workflow.tags.clone(),
        loop_config: workflow.loop_config.clone(),
        continue_on_error: false,
    };
    if let Err(e) = crate::validation::validate_create_workflow(
        &request,
        state.config.engine.max_loop_iterations,
        &generation.functions,
    ) {
        let problems: Vec<FieldError> = refusal_details(&e, "workflow")
            .into_iter()
            .filter(|d| !is_deferred(d))
            .collect();
        if !problems.is_empty() || deferred.is_empty() {
            return Ok(problems);
        }
    }
    if !deferred.is_empty() {
        return Ok(Vec::new());
    }
    let mut problems = Vec::new();
    let engine = &generation.engine;
    let row = definitions::workflow_row(workflow, "__definition__", 1)?;
    let json =
        crate::engine::workflow_json(&row, "__definition__", "__definition__", "active", None)?;
    if let Err(issues) = crate::engine::compile_row(&engine.compiler(), &json) {
        problems.extend(crate::validation::quarantine_refusals(issues));
    }
    Ok(problems)
}

/// A channel is checked as a channel create is, minus its identity and its
/// workflow, which the manifest supplies.
fn channel_problems(channel: &ChannelDefinition) -> Vec<FieldError> {
    let request = crate::storage::models::CreateChannelRequest {
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
        Err(e) => refusal_details(&e, "channel"),
    }
}

/// A definition-set validator's refusal as problems by field. Those
/// validators root their paths at `root` (`channel.config.…`,
/// `workflow.name`); a definition's fields sit directly under it.
fn refusal_details(error: &OrionError, root: &str) -> Vec<FieldError> {
    let prefix = format!("{root}.");
    let details: Vec<FieldError> = error
        .field_errors()
        .iter()
        .cloned()
        .map(|mut d| {
            if let Some(rest) = d.path.strip_prefix(&prefix) {
                d.path = rest.to_string();
            } else if d.path == root {
                d.path = String::new();
            }
            d
        })
        .collect();
    if details.is_empty() {
        vec![FieldError::new("", "INVALID", error.client_message())]
    } else {
        details
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
