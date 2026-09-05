//! Plugin gates and the resolution of an upload into a stored draft.
//!
//! The server never holds a path and never trusts a digest a client sent:
//! it receives bytes, hashes them, compiles them in the sandbox and probes
//! every declared function before a draft row exists — so a draft is already
//! known to load. A request that names a digest without bytes is accepted
//! only when this instance already holds that artifact, which is what an
//! export without `?include_artifacts=true` produces for a promotion.

use base64::Engine as _;
use serde_json::Value;

use crate::engine::FunctionRegistry;
use crate::errors::{FieldError, OrionError};
use crate::plugin::{Limits, Manifest, WasmRuntime};
use crate::server::state::AppState;
use crate::storage::models::Workflow;
use crate::storage::repositories::plugins::PluginDraft;

/// Everything about one request the synchronous half can decide: the
/// manifest, the bytes (decoded), and the digest they hash to.
pub(crate) struct Prepared {
    pub manifest: Manifest,
    pub manifest_json: Value,
    pub bytes: Option<Vec<u8>>,
    pub digest: String,
    pub tags: Vec<String>,
    /// Verified against `[plugins.trust]` when keys are configured; carried
    /// as sent otherwise, so a node without keys still stores what a signing
    /// pipeline produced and a node with keys can check it at load.
    pub signature: Option<String>,
}

fn refused(message: &str, details: Vec<FieldError>) -> OrionError {
    OrionError::Validation {
        code: orion_api::error::codes::VALIDATION_ERROR,
        message: message.to_string(),
        details,
    }
}

/// Parse and validate the manifest, decode the component, compute the
/// digest — no I/O, no sandbox. What the import's per-item validator runs.
pub(crate) fn prepare(
    config: &crate::config::PluginsConfig,
    plugin_id: Option<&str>,
    manifest: &Value,
    component: Option<&str>,
    digest: Option<&str>,
    signature: Option<&str>,
    tags: &[String],
) -> Result<Prepared, OrionError> {
    let parsed = match manifest {
        Value::String(text) => Manifest::parse(text),
        Value::Object(_) => match serde_json::from_value::<Manifest>(manifest.clone()) {
            Ok(m) => m.validated(),
            Err(e) => Err(vec![FieldError::new("manifest", "INVALID", e.to_string())]),
        },
        _ => Err(vec![FieldError::new(
            "manifest",
            "TYPE_MISMATCH",
            "manifest must be the TOML text (a string) or the manifest as a JSON object",
        )]),
    };
    let parsed = parsed.map_err(|details| {
        refused(
            "Plugin manifest is invalid",
            details
                .into_iter()
                .map(|d| {
                    FieldError::new(
                        format!("manifest.{}", d.path),
                        leak_code(&d.code),
                        d.message,
                    )
                })
                .collect(),
        )
    })?;
    if let Some(id) = plugin_id
        && id != parsed.name
    {
        return Err(refused(
            "plugin_id does not match the manifest",
            vec![FieldError::new(
                "plugin_id",
                "INVALID",
                format!(
                    "plugin_id '{id}' must equal the manifest's name '{}' — the manifest is the \
                     source of truth for the id",
                    parsed.name
                ),
            )],
        ));
    }
    let manifest_json = serde_json::to_value(&parsed)?;

    let bytes = match component {
        Some(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|e| {
                    refused(
                        "component is not valid base64",
                        vec![FieldError::new("component", "INVALID", e.to_string())],
                    )
                })?;
            if bytes.len() > config.max_component_bytes {
                return Err(refused(
                    "component is too large",
                    vec![FieldError::new(
                        "component",
                        "TOO_LONG",
                        format!(
                            "component is {} bytes, over plugins.max_component_bytes ({})",
                            bytes.len(),
                            config.max_component_bytes
                        ),
                    )],
                ));
            }
            Some(bytes)
        }
        None => None,
    };
    let computed = bytes.as_deref().map(WasmRuntime::digest);
    let digest = match (computed, digest) {
        (Some(computed), Some(claimed)) if computed != claimed => {
            return Err(refused(
                "digest does not match the component",
                vec![FieldError::new(
                    "digest",
                    "INVALID",
                    format!("the component hashes to {computed}, not {claimed}"),
                )],
            ));
        }
        (Some(computed), _) => computed,
        (None, Some(claimed)) => {
            if !claimed.starts_with("sha256:") || claimed.len() != 71 {
                return Err(refused(
                    "digest is not a component digest",
                    vec![FieldError::new(
                        "digest",
                        "INVALID",
                        "expected 'sha256:' followed by 64 hex characters",
                    )],
                ));
            }
            claimed.to_string()
        }
        (None, None) => {
            return Err(refused(
                "a component is required",
                vec![FieldError::new(
                    "component",
                    "REQUIRED",
                    "send the component as base64, or a digest this instance already holds",
                )],
            ));
        }
    };
    // The signature is over the digest — the identity the server just
    // computed or verified — so it is checked last, and only where the node
    // has keys to check it against. Its absence is a missing field, a bad one
    // is invalid; both name `signature`.
    if let Err(reason) = crate::plugin::trust::verify(&config.trust.public_keys, &digest, signature)
    {
        return Err(refused(
            "the component signature does not verify",
            vec![FieldError::new(
                "signature",
                if signature.is_none() {
                    "REQUIRED"
                } else {
                    "INVALID"
                },
                reason,
            )],
        ));
    }
    Ok(Prepared {
        manifest: parsed,
        manifest_json,
        bytes,
        digest,
        tags: tags.to_vec(),
        signature: signature
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// A manifest's field-error code, as the `'static` the constructor wants.
/// The manifest module emits a closed set, all of which the registry names.
fn leak_code(code: &str) -> &'static str {
    match code {
        "REQUIRED" => "REQUIRED",
        "DUPLICATE_FIELD" => "DUPLICATE_FIELD",
        "TYPE_MISMATCH" => "TYPE_MISMATCH",
        _ => "INVALID",
    }
}

/// The sandbox half: compile the bytes (or the stored artifact the digest
/// names), probe every declared function, and hand back the draft the
/// repository stores together with the bytes it must keep.
pub(crate) async fn resolve(
    state: &AppState,
    prepared: Prepared,
) -> Result<(PluginDraft, Option<Vec<u8>>), OrionError> {
    let Some(runtime) = &state.plugins else {
        return Err(OrionError::validation(
            "plugins are disabled on this node (plugins.enabled = false), so a component \
             cannot be compiled or checked here",
        ));
    };
    let bytes = match prepared.bytes {
        Some(bytes) => bytes,
        None => match runtime.cached(&prepared.digest) {
            // Already compiled on this node: nothing to fetch, and the load
            // below is a cache hit.
            Some(_) => Vec::new(),
            None => state
                .repos
                .plugins
                .get_artifact(&prepared.digest)
                .await?
                .ok_or_else(|| {
                    refused(
                        "no component is stored under that digest",
                        vec![FieldError::new(
                            "digest",
                            "INVALID",
                            format!(
                                "{} is not held by this instance — include the component",
                                prepared.digest
                            ),
                        )],
                    )
                })?,
        },
    };
    let loaded = match runtime.cached(&prepared.digest) {
        Some(loaded) => loaded,
        None => runtime.load(bytes.clone()).await.map_err(|e| {
            refused(
                "component failed to load",
                vec![FieldError::new("component", "INVALID", e.to_string())],
            )
        })?,
    };
    let limits = Limits::effective(&state.config.plugins, &prepared.manifest.name);
    let names: Vec<&str> = prepared.manifest.function_names().collect();
    runtime
        .self_test(&loaded, &limits, &names)
        .await
        .map_err(|e| {
            refused(
                "component failed its self-test",
                vec![FieldError::new("component", "INVALID", e.to_string())],
            )
        })?;
    let draft = PluginDraft {
        plugin_id: prepared.manifest.name.clone(),
        manifest_json: serde_json::to_string(&prepared.manifest_json)?,
        digest: prepared.digest,
        tags_json: serde_json::to_string(&prepared.tags)?,
        signature: prepared.signature,
    };
    // Bytes that came from the store need no second write.
    let keep = if bytes.is_empty() { None } else { Some(bytes) };
    Ok((draft, keep))
}

/// Every function name a workflow's tasks call.
pub(crate) fn called_functions(tasks: &Value) -> Vec<String> {
    crate::engine::leaf_tasks(tasks)
        .into_iter()
        .filter_map(|t| t.get("function")?.get("name")?.as_str().map(str::to_string))
        .collect()
}

/// The active workflows whose tasks name any of `functions`, sorted and
/// deduplicated — the dependants a plugin archive or delete is refused for.
pub(crate) async fn active_workflows_naming(
    workflows: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    functions: &[String],
) -> Result<Vec<String>, OrionError> {
    let mut users = Vec::new();
    for workflow in workflows.list_active().await? {
        let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
            continue;
        };
        if called_functions(&tasks)
            .iter()
            .any(|f| functions.contains(f))
        {
            users.push(workflow.workflow_id);
        }
    }
    users.sort();
    users.dedup();
    Ok(users)
}

/// Refuse to archive or delete a plugin while an active workflow still calls
/// one of its functions — a `409`, like a channel name another active row
/// holds: the conflict is with other rows' state, not with the request.
pub(crate) async fn ensure_no_active_dependants(
    workflows: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    plugin_id: &str,
    functions: &[String],
    verb: &str,
) -> Result<(), OrionError> {
    let users = active_workflows_naming(workflows, functions).await?;
    if users.is_empty() {
        return Ok(());
    }
    Err(OrionError::Conflict(format!(
        "Cannot {verb} plugin '{plugin_id}': active workflow(s) {} call its functions and \
         would be quarantined at the next reload. Archive or repoint them first.",
        users
            .iter()
            .map(|u| format!("'{u}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// A workflow activates only if every function it names is one this
/// generation dispatches. The create-time gate already refuses an unknown
/// name; this catches the name that *was* known — a plugin function whose
/// plugin has since been archived, or which this node could not load.
pub(crate) fn ensure_functions_available(
    functions: &FunctionRegistry,
    workflow: &Workflow,
) -> Result<(), OrionError> {
    let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
        return Ok(()); // unparseable tasks are caught elsewhere
    };
    let mut missing: Vec<String> = called_functions(&tasks)
        .into_iter()
        .filter(|f| !functions.contains(f))
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        return Ok(());
    }
    Err(OrionError::validation(format!(
        "Cannot activate workflow '{}': function(s) {} are not available on this node — \
         a plugin function needs its plugin active and loaded; check GET /plugins/{{id}} \
         and /health",
        workflow.workflow_id,
        missing
            .iter()
            .map(|m| format!("'{m}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// A plugin version activates only if every active workflow calling its
/// functions still satisfies the schema *this* version declares.
///
/// The other half of the workflow gate above. That one keeps a workflow from
/// activating against a function the generation lacks; this one keeps a
/// plugin from activating a schema its dependants no longer match — a field
/// renamed between versions, a new required one. Without it the activation
/// succeeds and the next reload quarantines every dependant, because the
/// handler checks the authored input against its table when the workflow
/// loads (`parse_input_with`). Refusing here says which workflow and which
/// field, while the previous version is still serving. A `409`, like the
/// archive gate: the conflict is with other rows' state.
///
/// Computed on function names, like the dependants list, against the entries
/// the row's manifest declares — the same table the create-time gate and the
/// handler read.
pub(crate) async fn ensure_dependants_accept(
    workflows: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    draft: &crate::storage::models::Plugin,
) -> Result<(), OrionError> {
    let manifest: crate::plugin::Manifest = serde_json::from_str(&draft.manifest_json)
        .map_err(|e| OrionError::validation(format!("stored manifest does not parse: {e}")))?;
    let binding = crate::engine::PluginBinding {
        id: draft.plugin_id.clone(),
        version: draft.version,
        digest: draft.digest.clone(),
        abi: manifest.abi.clone(),
    };
    let entries = manifest.entries(&binding);
    let functions: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let registry = FunctionRegistry::builtin()
        .with_entries(entries.clone())
        .map_err(OrionError::validation)?;

    let mut problems: Vec<String> = Vec::new();
    for workflow in workflows.list_active().await? {
        let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
            continue;
        };
        for (path, task) in crate::engine::walk_steps(&tasks).tasks {
            let Some(function) = task.get("function") else {
                continue;
            };
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !functions.contains(&name) {
                continue;
            }
            let input = function
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            for e in registry.validate_input(name, &input, &path) {
                problems.push(format!(
                    "workflow '{}' at {}: {} ({})",
                    workflow.workflow_id, e.path, e.message, e.code
                ));
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(OrionError::Conflict(format!(
        "Cannot activate plugin '{}' version {}: active workflow(s) call its functions with \
         inputs this version's schema does not accept, and would be quarantined at the next \
         reload — {}. Update those workflows first, or keep the previous version.",
        draft.plugin_id,
        draft.version,
        problems.join("; ")
    )))
}
