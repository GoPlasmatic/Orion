//! Model gates and the resolution of a registration into a stored draft.
//!
//! A model row never carries bytes. A registration names a storage
//! connector, an object key and the digest the bytes must hash to; the
//! synchronous half here ([`prepare`]) checks everything that needs no
//! network — the manifest, the id, the reference's shape, the signature over
//! the digest — and the asynchronous half ([`resolve`]) checks the connector
//! exists and allows reads, then asks the bucket whether the object is there
//! and how big it is. Fetching and verifying the bytes is admission, which a
//! worker runs after the row exists: it takes seconds, and a registration
//! should not.

use serde_json::Value;

use crate::errors::{FieldError, OrionError};
use crate::model::{ArtifactRef, FetchError, HeadInfo, Manifest};
use crate::server::state::AppState;
use crate::storage::models::Model;
use crate::storage::repositories::models::{ModelArtifactRef, ModelDraft};
use crate::storage::repositories::workflows::WorkflowRepository;

/// The task function that runs a model, and the input field that names it.
/// The function itself arrives with the runtime; the dependants walk reads
/// the authored task, so it needs only the spelling.
pub(crate) const INFER_FUNCTION: &str = "model_infer";
pub(crate) const INFER_MODEL_FIELD: &str = "model";

/// Everything about one registration the synchronous half can decide.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub manifest: Manifest,
    pub manifest_json: Value,
    pub artifact: ArtifactRef,
    pub tags: Vec<String>,
    /// Verified against `[models.trust]` when keys are configured; carried
    /// as sent otherwise, so a node without keys still stores what a signing
    /// pipeline produced and every admission re-verifies it.
    pub signature: Option<String>,
}

fn refused(message: &str, details: Vec<FieldError>) -> OrionError {
    OrionError::Validation {
        code: orion_api::error::codes::VALIDATION_ERROR,
        message: message.to_string(),
        details,
    }
}

/// Validate the manifest, the id and the artifact reference, and verify the
/// signature where the node has keys — no I/O. What the import's per-item
/// validator runs.
pub(crate) fn prepare(
    config: &crate::config::ModelsConfig,
    model_id: Option<&str>,
    manifest: &Value,
    artifact: &ModelArtifactRef,
    signature: Option<&str>,
    tags: &[String],
) -> Result<Prepared, OrionError> {
    let parsed = Manifest::validated(manifest).map_err(|details| {
        refused(
            "Model manifest is invalid",
            details
                .into_iter()
                .map(|d| {
                    let path = if d.path == "manifest" {
                        d.path
                    } else {
                        format!("manifest.{}", d.path)
                    };
                    FieldError::new(path, super::plugins::leak_code(&d.code), d.message)
                })
                .collect(),
        )
    })?;
    if let Some(id) = model_id
        && id != parsed.name
    {
        return Err(refused(
            "model_id does not match the manifest",
            vec![FieldError::new(
                "model_id",
                "INVALID",
                format!(
                    "model_id '{id}' must equal the manifest's name '{}' — the manifest is the \
                     source of truth for the id",
                    parsed.name
                ),
            )],
        ));
    }
    let manifest_json = serde_json::to_value(&parsed)?;

    // The wire type is tolerant — a missing field parses to an empty string
    // — so the shape is checked here, every problem at once.
    let mut problems = Vec::new();
    for (field, value) in [
        ("connector", &artifact.connector),
        ("key", &artifact.key),
        ("digest", &artifact.digest),
    ] {
        if value.trim().is_empty() {
            problems.push(FieldError::new(
                format!("artifact.{field}"),
                "REQUIRED",
                format!("artifact.{field} is required"),
            ));
        }
    }
    if !artifact.digest.trim().is_empty() && !crate::crypto::is_sha256_digest(&artifact.digest) {
        problems.push(FieldError::new(
            "artifact.digest",
            "INVALID",
            format!(
                "digest '{}' is not an artifact digest: expected 'sha256:' followed by 64 \
                 lowercase hex characters",
                artifact.digest
            ),
        ));
    }
    if !problems.is_empty() {
        return Err(refused("artifact reference is incomplete", problems));
    }
    let artifact = ArtifactRef {
        connector: artifact.connector.trim().to_string(),
        key: artifact.key.trim().to_string(),
        digest: artifact.digest.trim().to_string(),
        size: artifact.size,
    };

    // The signature is over the claimed digest, so it is checked last and
    // only where the node has keys to check it against. Its absence is a
    // missing field, a bad one is invalid; both name `signature`.
    if let Err(reason) =
        crate::crypto::ed25519::verify(&config.trust.public_keys, &artifact.digest, signature)
    {
        return Err(refused(
            "the artifact signature does not verify",
            vec![FieldError::new(
                "signature",
                if signature.is_none() {
                    "REQUIRED"
                } else {
                    "INVALID"
                },
                format!("{reason} (models.trust.public_keys)"),
            )],
        ));
    }
    Ok(Prepared {
        manifest: parsed,
        manifest_json,
        artifact,
        tags: tags.to_vec(),
        signature: signature
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// The refusal every model route answers on a node without the runtime.
pub(crate) fn disabled() -> OrionError {
    OrionError::validation(
        "models are disabled on this node (models.enabled = false), so a model cannot be \
         registered, checked or admitted here",
    )
}

/// The network half: the connector must exist, be a storage connector and
/// allow reads, and the object must be there and within
/// `models.max_artifact_bytes`. Hands back the draft the repository stores
/// and what the bucket said about the object.
pub(crate) async fn resolve(
    state: &AppState,
    prepared: Prepared,
) -> Result<(ModelDraft, HeadInfo), OrionError> {
    let Some(models) = &state.models else {
        return Err(disabled());
    };
    let name = &prepared.artifact.connector;
    let storage = match state.connector_registry.get(name).await {
        None => {
            return Err(refused(
                "artifact connector is not available",
                vec![FieldError::new(
                    "artifact.connector",
                    "INVALID",
                    format!(
                        "storage connector '{name}' does not exist, is disabled, or failed to \
                         load on this node"
                    ),
                )],
            ));
        }
        Some(config) => match config.as_ref() {
            crate::connector::ConnectorConfig::Storage(storage) => storage.clone(),
            other => {
                return Err(refused(
                    "artifact connector is not a storage connector",
                    vec![FieldError::new(
                        "artifact.connector",
                        "INVALID",
                        format!(
                            "connector '{name}' is a {} connector; a model artifact is read \
                             through a storage connector",
                            other.connector_type().as_str()
                        ),
                    )],
                ));
            }
        },
    };
    if !storage.operations.presign_get {
        return Err(refused(
            "artifact connector does not allow reads",
            vec![FieldError::new(
                "artifact.connector",
                "INVALID",
                format!(
                    "storage connector '{name}' does not allow reads (operations.presign_get = \
                     false), and a model fetch is a signed GET"
                ),
            )],
        ));
    }
    let head = models
        .store
        .head(&storage, &state.http_client, &prepared.artifact.key)
        .await
        .map_err(|e| head_refusal(&prepared.artifact, e))?;
    let limit = state.config.models.max_artifact_bytes;
    if let Some(size) = head.size
        && size > limit as u64
    {
        return Err(refused(
            "artifact is too large",
            vec![FieldError::new(
                "artifact",
                "TOO_LONG",
                format!(
                    "the object at key '{}' is {size} bytes, over models.max_artifact_bytes \
                     ({limit})",
                    prepared.artifact.key
                ),
            )],
        ));
    }
    // The size the bucket reports is the one worth storing; the author's is
    // the fallback when the bucket did not say.
    let stored = ArtifactRef {
        size: head.size.or(prepared.artifact.size),
        ..prepared.artifact
    };
    let draft = ModelDraft {
        model_id: prepared.manifest.name.clone(),
        manifest_json: serde_json::to_string(&prepared.manifest_json)?,
        artifact_json: serde_json::to_string(&stored)?,
        digest: stored.digest.clone(),
        tags_json: serde_json::to_string(&prepared.tags)?,
        signature: prepared.signature,
    };
    Ok((draft, head))
}

/// A HEAD that did not answer 200, as a refusal naming the key.
fn head_refusal(artifact: &ArtifactRef, error: FetchError) -> OrionError {
    refused(
        "artifact object could not be checked",
        vec![FieldError::new(
            "artifact.key",
            "INVALID",
            format!(
                "connector '{}', key '{}': {error} ({} stage)",
                artifact.connector,
                artifact.key,
                error.stage()
            ),
        )],
    )
}

/// One active workflow that names a model, and where.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ModelDependant {
    pub workflow_id: String,
    pub version: i64,
    /// The ids of the tasks calling `model_infer` with this model as a
    /// literal `input.model`.
    pub task_ids: Vec<String>,
}

/// The ids of the tasks in `tasks` that call `model_infer` with `model_id`
/// as a literal `input.model`. A computed reference — an expression, a
/// template — is not seen: the model it resolves to is decided per message.
pub(crate) fn literal_references(tasks: &Value, model_id: &str) -> Vec<String> {
    crate::engine::walk_steps(tasks)
        .tasks
        .into_iter()
        .filter_map(|(path, task)| {
            let function = task.get("function")?;
            if function.get("name").and_then(Value::as_str) != Some(INFER_FUNCTION) {
                return None;
            }
            let named = function.get("input")?.get(INFER_MODEL_FIELD)?.as_str()?;
            (named == model_id).then(|| {
                task.get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or(path)
            })
        })
        .collect()
}

/// The active workflows whose tasks name `model_id` literally, in id order —
/// the dependants an archive or delete is refused for.
pub(crate) async fn active_workflows_naming(
    workflows: &dyn WorkflowRepository,
    model_id: &str,
) -> Result<Vec<ModelDependant>, OrionError> {
    let mut users = Vec::new();
    for workflow in workflows.list_active().await? {
        let Ok(tasks) = serde_json::from_str::<Value>(&workflow.tasks_json) else {
            continue;
        };
        let task_ids = literal_references(&tasks, model_id);
        if !task_ids.is_empty() {
            users.push(ModelDependant {
                workflow_id: workflow.workflow_id,
                version: workflow.version,
                task_ids,
            });
        }
    }
    users.sort_by(|a, b| {
        a.workflow_id
            .cmp(&b.workflow_id)
            .then(a.version.cmp(&b.version))
    });
    Ok(users)
}

/// Refuse to archive or delete a model while an active workflow names it —
/// a `409`, like a plugin's: the conflict is with other rows' state.
pub(crate) async fn ensure_no_active_dependants(
    workflows: &dyn WorkflowRepository,
    model_id: &str,
    verb: &str,
) -> Result<(), OrionError> {
    let users = active_workflows_naming(workflows, model_id).await?;
    if users.is_empty() {
        return Ok(());
    }
    Err(OrionError::Conflict(format!(
        "Cannot {verb} model '{model_id}': active workflow(s) {} call model_infer on it and \
         would be quarantined at the next reload. Archive or repoint them first.",
        users
            .iter()
            .map(|u| format!("'{}'", u.workflow_id))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Whether the active workflows naming this version still fit it.
///
/// Always `Ok` for now, and deliberately so: the plugin twin checks each
/// dependant's authored input against the version's schema, but what a
/// `model_infer` task hands a model is decided by the manifest's adapters
/// at inference time, not by the task's input shape — so there is no
/// authored input to check a declared input against here. When the task
/// function exists, a version whose adapters no longer produce what the
/// graph expects is caught by admission's probe, on the row, before this
/// gate runs.
pub(crate) fn ensure_dependants_accept(_draft: &Model) -> Result<(), OrionError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn infer(id: &str, model: Value) -> Value {
        json!({"id": id, "name": id, "function": {"name": "model_infer",
            "input": {"model": model, "output": "data.out"}}})
    }

    /// Literal references are found through task groups; computed ones and
    /// other functions are not references.
    #[test]
    fn literal_references_walk_groups_and_skip_computed_ones() {
        let tasks = json!([
            infer("plain", json!("ada.c4-tiny")),
            {"id": "group", "tasks": [infer("nested", json!("ada.c4-tiny"))]},
            infer("computed", json!({"var": "data.model"})),
            infer("other", json!("ada.other")),
            {"id": "log", "name": "log", "function": {"name": "log",
                "input": {"message": "ada.c4-tiny"}}},
        ]);
        assert_eq!(
            literal_references(&tasks, "ada.c4-tiny"),
            vec!["plain".to_string(), "nested".to_string()]
        );
        assert!(literal_references(&tasks, "ada.none").is_empty());
    }

    /// The shape checks report every missing field at once, and a bad digest
    /// by name.
    #[test]
    fn an_incomplete_reference_names_every_missing_field() {
        let manifest = json!({
            "abi": "orion:model@1.0.0", "name": "ada.c4-tiny", "version": "1",
            "inputs": [{"name": "board", "dtype": "f32", "shape": [1, 2, 6, 7]}],
            "outputs": [{"name": "policy", "dtype": "f32", "shape": [1, 7]}]
        });
        let config = crate::config::ModelsConfig::default();
        let err = prepare(
            &config,
            None,
            &manifest,
            &ModelArtifactRef {
                connector: String::new(),
                key: String::new(),
                digest: "sha256:short".to_string(),
                size: None,
            },
            None,
            &[],
        )
        .expect_err("incomplete");
        let OrionError::Validation { details, .. } = err else {
            unreachable!("validation")
        };
        let mut paths: Vec<(String, String)> =
            details.into_iter().map(|d| (d.path, d.code)).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                ("artifact.connector".to_string(), "REQUIRED".to_string()),
                ("artifact.digest".to_string(), "INVALID".to_string()),
                ("artifact.key".to_string(), "REQUIRED".to_string()),
            ]
        );

        // A manifest problem is reported under `manifest.`.
        let err = prepare(
            &config,
            Some("ada.other"),
            &json!({"abi": "orion:model@1.0.0", "name": "ada.c4-tiny", "version": "1",
                "inputs": [], "outputs": []}),
            &ModelArtifactRef::default(),
            None,
            &[],
        )
        .expect_err("no inputs");
        let OrionError::Validation { details, .. } = err else {
            unreachable!("validation")
        };
        assert_eq!(details[0].path, "manifest.inputs");
        assert_eq!(details[0].code, "REQUIRED");
    }
}
