//! `/api/v1/admin/plugins` — the plugin entity, mirroring the workflow
//! surface: create as a draft, list, get, update, delete, status, versions,
//! dependencies, import, export, validate.
//!
//! What differs from a workflow is the upload. A request carries the
//! manifest and the component (as base64, or as a digest this instance
//! already holds); the server validates the manifest, hashes the bytes,
//! compiles them in the sandbox and probes every declared function before
//! the draft row exists, so a draft is already known to load. See
//! `services::plugins` for that resolution and for the gates.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::plugin::Manifest;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{DataEnvelope, PaginatedEnvelope};
use crate::server::routes::response_helpers::{created_response, data_response, paginated_into};
use crate::server::state::AppState;
use crate::storage::models::{Plugin, PluginResponse};
use crate::storage::repositories::helpers::VersionFilter;
use crate::storage::repositories::plugins::{
    CreatePluginRequest, PluginFilter, PluginRepository, UpdatePluginRequest,
};
use crate::storage::repositories::workflows::StatusChangeRequest;

use super::StatusAction;
use super::services::plugins as svc;
use super::{ValidationEnvelope, audit_log_draft_only, issues_from_error};

/// The function names a stored row declares.
fn functions_of(row: &Plugin) -> Vec<String> {
    serde_json::from_str::<Manifest>(&row.manifest_json)
        .map(|m| m.function_names().map(str::to_string).collect())
        .unwrap_or_default()
}

/// A request's plugin id without resolving it: `plugin_id` when given, else
/// the manifest's `name` — read off the object, or parsed out of the TOML.
fn manifest_name(req: &CreatePluginRequest) -> Option<String> {
    if let Some(id) = &req.plugin_id {
        return Some(id.clone());
    }
    match &req.manifest {
        Value::Object(o) => o.get("name").and_then(Value::as_str).map(str::to_string),
        Value::String(text) => toml::from_str::<toml::Value>(text).ok().and_then(|v| {
            v.get("name")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        }),
        _ => None,
    }
}

/// `prepare` then `resolve`: the whole path from a request to a stored draft.
async fn resolve_request(
    state: &AppState,
    req: &CreatePluginRequest,
) -> Result<
    (
        crate::storage::repositories::plugins::PluginDraft,
        Option<Vec<u8>>,
    ),
    OrionError,
> {
    let prepared = svc::prepare(
        &state.config.plugins,
        req.plugin_id.as_deref(),
        &req.manifest,
        req.component.as_deref(),
        req.digest.as_deref(),
        req.signature.as_deref(),
        &req.tags,
    )?;
    svc::resolve(state, prepared).await
}

/// An upload body: JSON, or `multipart/form-data` for a component too large
/// to base64 comfortably.
///
/// The multipart form carries the same fields as the JSON shape, one part
/// each — `manifest` (the TOML text), `component` (the raw bytes),
/// `plugin_id`, `digest`, `signature`, and `tags` as a JSON array or
/// repeated once per tag. The parts are folded into the JSON shape and
/// deserialised through it, so the two forms cannot accept different things.
pub(crate) struct PluginUpload<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for PluginUpload<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OrionError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, OrionError> {
        let is_multipart = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));
        if !is_multipart {
            let OrionJson(value) = OrionJson::<T>::from_request(req, state).await?;
            return Ok(Self(value));
        }
        let mut multipart = axum::extract::Multipart::from_request(req, state)
            .await
            .map_err(|e| OrionError::validation(format!("multipart body: {e}")))?;
        let mut body = serde_json::Map::new();
        let mut tags: Vec<Value> = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| OrionError::validation(format!("multipart body: {e}")))?
        {
            let name = field.name().unwrap_or_default().to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| OrionError::validation(format!("multipart part '{name}': {e}")))?;
            match name.as_str() {
                "component" => {
                    body.insert(
                        name,
                        json!(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                    );
                }
                "tags" => {
                    let text = String::from_utf8_lossy(&bytes);
                    match serde_json::from_str::<Value>(&text) {
                        Ok(Value::Array(items)) => tags.extend(items),
                        _ => tags.push(json!(text.trim())),
                    }
                }
                "manifest" | "plugin_id" | "digest" | "signature" => {
                    let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
                        OrionError::validation(format!("multipart part '{name}' is not UTF-8"))
                    })?;
                    body.insert(name, json!(text));
                }
                other => {
                    return Err(OrionError::validation(format!(
                        "multipart part '{other}' is not a plugin upload field (expected \
                         manifest, component, plugin_id, digest, signature or tags)"
                    )));
                }
            }
        }
        if !tags.is_empty() {
            body.insert("tags".to_string(), Value::Array(tags));
        }
        let value: T = serde_json::from_value(Value::Object(body))
            .map_err(|e| OrionError::validation(format!("multipart upload: {e}")))?;
        Ok(Self(value))
    }
}

// ============================================================
// Plugins CRUD
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins",
    params(PluginFilter),
    tag = "Plugins",
    responses(
        (status = 200, description = "Paginated list of plugins", body = PaginatedEnvelope<PluginResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_plugins(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<PluginFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.plugins.list_paginated(&filter).await?;
    paginated_into(result, |p| PluginResponse::try_from(p))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins",
    tag = "Plugins",
    request_body = CreatePluginRequest,
    responses(
        (status = 201, description = "Plugin created as draft. The manifest was validated, the \
            component hashed, compiled in the sandbox and every declared function probed before \
            the row was written, so the draft is already known to load.", body = DataEnvelope<PluginResponse>),
        (status = 400, description = "Invalid manifest, component, or digest; or plugins are disabled on this node"),
        (status = 409, description = "Plugin id already exists"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_plugin(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    PluginUpload(req): PluginUpload<CreatePluginRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let (draft, bytes) = resolve_request(&state, &req).await?;
    let plugin = state.repos.plugins.create(&draft, bytes.as_deref()).await?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create",
        "plugin",
        &plugin.plugin_id,
    );
    Ok(created_response(PluginResponse::try_from(&plugin)?))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/{id}",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "The latest version, with this node's load state under `health`", body = DataEnvelope<PluginResponse>),
        (status = 404, description = "Plugin not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_plugin(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let plugin = state.repos.plugins.get_by_id(&id).await?;
    let mut response = PluginResponse::try_from(&plugin)?;
    let generation = state.runtime.load();
    response.health = Some(generation.plugins.health_of(
        &plugin.plugin_id,
        plugin.version,
        state.plugins.is_some(),
    ));
    Ok(data_response(response))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/plugins/{id}",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID")),
    request_body = UpdatePluginRequest,
    responses(
        (status = 200, description = "Draft plugin updated; an absent field keeps its stored value", body = DataEnvelope<PluginResponse>),
        (status = 400, description = "Invalid input"),
        (status = 404, description = "Plugin not found, or it has no draft version to update"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_plugin(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    PluginUpload(req): PluginUpload<UpdatePluginRequest>,
) -> Result<Json<Value>, OrionError> {
    let existing = state.repos.plugins.get_by_id(&id).await?;
    let manifest = match req.manifest {
        Some(m) => m,
        None => serde_json::from_str(&existing.manifest_json)?,
    };
    let tags: Vec<String> = match req.tags {
        Some(t) => t,
        None => serde_json::from_str(&existing.tags_json)?,
    };
    // A new component replaces the old one; otherwise the digest is the
    // stored one unless the request names another this instance holds.
    let (component, digest) = match (req.component, req.digest) {
        (Some(c), d) => (Some(c), d),
        (None, Some(d)) => (None, Some(d)),
        (None, None) => (None, Some(existing.digest.clone())),
    };
    // The stored signature carries over an edit that keeps the digest; one
    // that changes the component needs a new signature, and a node with keys
    // says so when the old one no longer verifies.
    let signature = req.signature.or_else(|| existing.signature.clone());
    let prepared = svc::prepare(
        &state.config.plugins,
        Some(&id),
        &manifest,
        component.as_deref(),
        digest.as_deref(),
        signature.as_deref(),
        &tags,
    )?;
    let (draft, bytes) = svc::resolve(&state, prepared).await?;
    let plugin = state
        .repos
        .plugins
        .replace_draft(&id, &draft, bytes.as_deref())
        .await?;
    audit_log_draft_only(&state.audit_queue, &principal, "update", "plugin", &id);
    Ok(data_response(PluginResponse::try_from(&plugin)?))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/plugins/{id}",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 204, description = "Plugin deleted (all versions), and any component nothing names any more"),
        (status = 404, description = "Plugin not found"),
        (status = 409, description = "An active workflow still calls one of its functions"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_plugin(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    let latest = state.repos.plugins.get_by_id(&id).await?;
    svc::ensure_no_active_dependants(
        state.repos.workflows.as_ref(),
        &id,
        &functions_of(&latest),
        "delete",
    )
    .await?;
    // §2.6: the delete and its audit row commit together.
    let mut write = super::audited_write(&state, &principal, "delete", "plugin", &id).await?;
    state.repos.plugins.delete_tx(write.tx(), &id).await?;
    write.commit().await?;

    super::reload_after_commit_scoped(
        &state,
        super::ReloadMode::Now,
        crate::cluster::EpochScope::Plugins,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Plugin Status Management
// ============================================================

#[utoipa::path(
    patch,
    path = "/api/v1/admin/plugins/{id}/status",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID"), super::StatusChangeQuery),
    request_body = StatusChangeRequest,
    responses(
        (status = 200, description = "Status updated. Activating supersedes the previously active \
            version in the same transaction, so a function name resolves to one digest per \
            generation; it is refused while an active workflow calls one of the version's \
            functions with an input its schema does not accept. Archiving is refused while an \
            active workflow calls one of the plugin's functions. `?dry_run=true` reports every \
            gate without writing; `?reload=defer` commits without rebuilding the engine.",
            body = DataEnvelope<PluginResponse>),
        (status = 400, description = "Invalid status transition, or plugins are disabled on this node"),
        (status = 404, description = "Plugin not found"),
        (status = 409, description = "An active workflow still calls one of its functions, or calls \
            one with an input the version being activated does not accept"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn change_plugin_status(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::StatusChangeQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<StatusChangeRequest>,
) -> Result<Json<Value>, OrionError> {
    let action = StatusAction::parse(req.status)?;
    let lifecycle = PluginLifecycle {
        plugins: state.repos.plugins.as_ref(),
        workflows: state.repos.workflows.as_ref(),
        sandbox: state.plugins.is_some(),
    };
    // Archiving has a gate of its own: the dependants. Checked before the
    // transaction opens, like the activation gates.
    let archive_gate = async {
        if !matches!(action, StatusAction::Archive) {
            return Ok(());
        }
        let latest = state.repos.plugins.get_by_id(&id).await?;
        svc::ensure_no_active_dependants(
            state.repos.workflows.as_ref(),
            &id,
            &functions_of(&latest),
            "archive",
        )
        .await
    };
    if query.dry_run {
        let mut errors = super::status_change_findings(&lifecycle, &id, &action).await?;
        if let Err(e) = archive_gate.await {
            errors.extend(issues_from_error(e));
        }
        let envelope = ValidationEnvelope::new(errors, Vec::new());
        return Ok(Json(serde_json::to_value(envelope)?));
    }
    if matches!(action, StatusAction::Activate) {
        let draft = state.repos.plugins.get_by_id(&id).await?;
        super::check_activation(&lifecycle, &draft).await?;
    }
    archive_gate.await?;

    let mut write = super::audited_write(
        &state,
        &principal,
        &format!("status_{}", req.status),
        "plugin",
        &id,
    )
    .await?;
    let plugin = match action {
        StatusAction::Activate => state.repos.plugins.activate_tx(write.tx(), &id).await?,
        StatusAction::Archive => state.repos.plugins.archive_tx(write.tx(), &id).await?,
    };
    write.commit().await?;

    super::reload_after_commit_scoped(&state, query.reload, crate::cluster::EpochScope::Plugins)
        .await?;
    Ok(data_response(PluginResponse::try_from(&plugin)?))
}

/// [`VersionedLifecycle`](super::VersionedLifecycle) for plugins.
///
/// Two activation gates, both about whether the functions would exist once
/// active: the sandbox must be on for this node, and the component the draft
/// names must be stored. Compiling and probing happened at upload.
struct PluginLifecycle<'a> {
    plugins: &'a dyn PluginRepository,
    workflows: &'a dyn crate::storage::repositories::workflows::WorkflowRepository,
    sandbox: bool,
}

impl super::VersionedLifecycle for PluginLifecycle<'_> {
    type Row = Plugin;
    const NOUN: &'static str = "plugin";

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.plugins.get_by_id(id).await
    }

    async fn has_active(&self, id: &str) -> Result<bool, OrionError> {
        Ok(self
            .plugins
            .list_active()
            .await?
            .iter()
            .any(|p| p.plugin_id == id))
    }

    async fn activation_gates(&self, draft: &Self::Row) -> Vec<OrionError> {
        let mut gates = Vec::new();
        if !self.sandbox {
            gates.push(OrionError::validation(format!(
                "Cannot activate plugin '{}': plugins are disabled on this node \
                 (plugins.enabled = false), so its functions could not be loaded",
                draft.plugin_id
            )));
        }
        match self.plugins.artifact_exists(&draft.digest).await {
            Ok(true) => {}
            Ok(false) => gates.push(OrionError::validation(format!(
                "Cannot activate plugin '{}': no component is stored under {} — \
                 update the draft with the component",
                draft.plugin_id, draft.digest
            ))),
            Err(e) => gates.push(e),
        }
        // The dependants must still fit this version's schema — checked
        // last, because it is the one gate whose failure names other rows.
        if let Err(e) = svc::ensure_dependants_accept(self.workflows, draft).await {
            gates.push(e);
        }
        gates
    }
}

// ============================================================
// Plugin Version Management and dependencies
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/{id}/versions",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID"), VersionFilter),
    responses(
        (status = 200, description = "Paginated version history", body = PaginatedEnvelope<PluginResponse>),
        (status = 404, description = "Plugin not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_plugin_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(filter): OrionQuery<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    let _ = state.repos.plugins.get_by_id(&id).await?;
    let result = state.repos.plugins.list_versions(&id, &filter).await?;
    paginated_into(result, |p| PluginResponse::try_from(p))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/{id}/versions",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 201, description = "New draft version copied from the latest", body = DataEnvelope<PluginResponse>),
        (status = 409, description = "Draft already exists"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_new_plugin_version(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let plugin = state.repos.plugins.create_new_version(&id).await?;
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "create_version",
        "plugin",
        &id,
    );
    Ok(created_response(PluginResponse::try_from(&plugin)?))
}

/// What depends on a plugin: the active workflows calling its functions.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PluginDependencies {
    plugin_id: String,
    version: i64,
    functions: Vec<String>,
    /// Active workflows whose tasks call any of `functions` — the ones an
    /// archive or delete is refused for.
    workflows: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/{id}/dependencies",
    tag = "Plugins",
    params(("id" = String, Path, description = "Plugin ID")),
    responses(
        (status = 200, description = "The functions the latest version declares and the active workflows calling them", body = DataEnvelope<PluginDependencies>),
        (status = 404, description = "Plugin not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn plugin_dependencies(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let plugin = state.repos.plugins.get_by_id(&id).await?;
    let functions = functions_of(&plugin);
    let workflows =
        svc::active_workflows_naming(state.repos.workflows.as_ref(), &functions).await?;
    Ok(data_response(PluginDependencies {
        plugin_id: plugin.plugin_id,
        version: plugin.version,
        functions,
        workflows,
    }))
}

// ============================================================
// Plugin Import / Export / Validation
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/import",
    tag = "Plugins",
    request_body = Vec<CreatePluginRequest>,
    params(super::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Each item is handled independently. An item may carry its component inline (base64) or \
            name a digest this instance already holds — what an export produces with and without \
            `?include_artifacts=true`. `?on_conflict=new_version` upserts: an existing draft is \
            replaced, an active plugin whose content differs gets a new draft version, identical \
            content is reported `unchanged`.", body = DataEnvelope<orion_api::ImportResult>),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_plugins(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    super::check_import_batch_size(items.len())?;
    let config = state.config.clone();
    let probe = state.repos.plugins.clone();
    let create_state = state.clone();
    let upsert_state = state.clone();
    let outcome = super::import_items::<CreatePluginRequest, _, _, _, _, _, _, _, _>(
        items,
        query.dry_run,
        query.on_conflict,
        super::ImportOps {
            // The synchronous half: manifest, base64, size, digest. The
            // sandbox half runs when the item is written.
            validate: |p: &CreatePluginRequest| {
                svc::prepare(
                    &config.plugins,
                    p.plugin_id.as_deref(),
                    &p.manifest,
                    p.component.as_deref(),
                    p.digest.as_deref(),
                    p.signature.as_deref(),
                    &p.tags,
                )
                .map(|_| ())
            },
            conflict_key: manifest_name,
            exists: |id: String| {
                let repo = probe.clone();
                async move { super::workflows::exists_or_err(repo.get_by_id(&id).await) }
            },
            create: |p: CreatePluginRequest| {
                let state = create_state.clone();
                async move {
                    let (draft, bytes) = resolve_request(&state, &p).await?;
                    state
                        .repos
                        .plugins
                        .create(&draft, bytes.as_deref())
                        .await
                        .map(|_| ())
                }
            },
            upsert: |p: CreatePluginRequest, dry_run: bool| {
                let state = upsert_state.clone();
                async move { super::versioned_upsert(&PluginUpsert(&state), p, dry_run).await }
            },
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }
    for id in outcome.written() {
        audit_log_draft_only(&state.audit_queue, &principal, "import", "plugin", id);
    }
    audit_log_draft_only(
        &state.audit_queue,
        &principal,
        "import",
        "plugin",
        &format!("{} imported", outcome.imported),
    );
    Ok(super::import_response(false, outcome))
}

/// [`super::VersionedUpsert`] for plugins.
struct PluginUpsert<'a>(&'a AppState);

impl super::VersionedUpsert for PluginUpsert<'_> {
    type Row = Plugin;
    type Request = CreatePluginRequest;

    fn request_id(req: &Self::Request) -> Option<String> {
        manifest_name(req)
    }

    fn row_status(row: &Self::Row) -> &str {
        &row.status
    }

    fn content_matches(row: &Self::Row, req: &Self::Request) -> Result<bool, OrionError> {
        // `content_matches` is synchronous, and the digest is decidable
        // without the sandbox: it is the hash of the bytes, or the digest
        // the item names.
        let prepared = svc::prepare(
            &crate::config::PluginsConfig {
                // Only the size ceiling is consulted here; the sandbox half
                // is not run. The real config's ceiling applies at write.
                max_component_bytes: usize::MAX,
                ..crate::config::PluginsConfig::default()
            },
            req.plugin_id.as_deref(),
            &req.manifest,
            req.component.as_deref(),
            req.digest.as_deref(),
            // Not checked here — the default config names no keys — and not
            // part of the content either: the digest is the identity.
            req.signature.as_deref(),
            &req.tags,
        )?;
        Ok(crate::storage::content::plugin_content(row)?
            == crate::storage::content::plugin_request_content(
                &prepared.manifest_json,
                &prepared.digest,
                &prepared.tags,
            ))
    }

    async fn create(&self, req: &Self::Request) -> Result<(), OrionError> {
        let (draft, bytes) = resolve_request(self.0, req).await?;
        self.0
            .repos
            .plugins
            .create(&draft, bytes.as_deref())
            .await
            .map(|_| ())
    }

    async fn get_by_id(&self, id: &str) -> Result<Self::Row, OrionError> {
        self.0.repos.plugins.get_by_id(id).await
    }

    async fn create_new_version(&self, id: &str) -> Result<(), OrionError> {
        self.0
            .repos
            .plugins
            .create_new_version(id)
            .await
            .map(|_| ())
    }

    async fn replace_draft(&self, id: &str, req: &Self::Request) -> Result<(), OrionError> {
        let (draft, bytes) = resolve_request(self.0, req).await?;
        self.0
            .repos
            .plugins
            .replace_draft(id, &draft, bytes.as_deref())
            .await
            .map(|_| ())
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/export",
    tag = "Plugins",
    params(PluginFilter),
    responses(
        (status = 200, description = "Exported plugins, importable as they are. With \
            `?include_artifacts=true` each item also carries its component as base64 under \
            `component`; without it the item names only the digest, which an import accepts \
            when the target already holds the artifact.", body = DataEnvelope<Vec<PluginResponse>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_plugins(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<PluginFilter>,
) -> Result<Json<Value>, OrionError> {
    let rows = state.repos.plugins.snapshot(&filter).await?;
    let mut data = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut item = serde_json::to_value(PluginResponse::try_from(row)?)?;
        if filter.include_artifacts.unwrap_or(false)
            && let Some(bytes) = state.repos.plugins.get_artifact(&row.digest).await?
        {
            item["component"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        data.push(item);
    }
    Ok(data_response(data))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/validate",
    tag = "Plugins",
    request_body = CreatePluginRequest,
    responses(
        (status = 200, description = "Validation result: `valid: true` means `POST /plugins` would \
            accept this payload on this node — the manifest parses, the component compiles and \
            every declared function answers a probe.", body = super::ValidationResponse),
    )
)]
#[tracing::instrument(skip(state, req))]
pub(crate) async fn validate_plugin(
    State(state): State<AppState>,
    PluginUpload(req): PluginUpload<CreatePluginRequest>,
) -> Result<Json<ValidationEnvelope>, OrionError> {
    let errors = match resolve_request(&state, &req).await {
        Ok(_) => Vec::new(),
        Err(e) => issues_from_error(e),
    };
    Ok(Json(ValidationEnvelope::new(errors, Vec::new())))
}
