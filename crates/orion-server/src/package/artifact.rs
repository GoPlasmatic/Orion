//! The package artifact: one JSON document carrying a package's header,
//! its `requires` boundary and its members in the shapes the `/import`
//! endpoints accept — and the content hash computed over them.
//!
//! `package.content_hash` is computed over the entities' *importable
//! content* — each entry projected through the same `storage::content`
//! canonicalization the server hashes with (K10) — so DB-owned fields
//! (`status`, `version`, timestamps) never make two artifacts differ.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Error;
use crate::storage::content;
use crate::storage::repositories::channels::CreateChannelRequest;
use crate::storage::repositories::connectors::CreateConnectorRequest;
use crate::storage::repositories::plugins::CreatePluginRequest;
use crate::storage::repositories::workflows::CreateWorkflowRequest;

#[derive(Debug, Serialize, Deserialize)]
pub struct PackageArtifact {
    pub package: PackageMeta,
    #[serde(default)]
    pub requires: Requires,
    /// The fourth member: plugins, each in the shape `/plugins/import`
    /// accepts — `plugin_id`, `manifest`, `digest`, `tags`, and the component
    /// as base64 when the export carried it. Omitted from the document *and*
    /// the hash when empty, so a package without plugins hashes exactly as it
    /// did before plugins existed and every applied receipt stays valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<Value>,
    /// The fifth member: models, each in the shape `/models/import` accepts
    /// — `model_id`, `manifest`, `artifact` (`connector`, `key`, `digest`),
    /// `tags` — and never the bytes: the target fetches them through the
    /// connector at admission. Omitted from the document *and* the hash
    /// when empty, for the same reason as `plugins`: a package without
    /// models must hash exactly as it did before the member existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<Value>,
    #[serde(default)]
    pub connectors: Vec<Value>,
    #[serde(default)]
    pub workflows: Vec<Value>,
    #[serde(default)]
    pub channels: Vec<Value>,
}

/// A plugin the package uses but does not carry: the target must hold this
/// digest active under this id before the package can apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRequirement {
    pub id: String,
    pub digest: String,
}

/// A model the package's workflows name and the artifact does not carry:
/// the target must serve it active. `version` and `digest` are what the
/// source held when it could say — `0` and empty when the source had no
/// row at all — and `plan` checks the digest only when one is named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequirement {
    pub id: String,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMeta {
    pub name: String,
    pub version: String,
    /// The Orion version that exported this artifact — informational.
    #[serde(default)]
    pub orion: String,
    pub content_hash: String,
    /// Where the artifact came from: a server URL for `export`, a directory
    /// for `compile`. Informational.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exported_from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exported_at: String,
}

/// Declared external dependencies: names this package uses but
/// deliberately does not contain, so closures stay small. `plan` verifies
/// they exist and are active in the target.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Requires {
    /// The Orion version range the definition set declared
    /// (`package.requires.orion`), or `--requires-orion`. `plan` and `apply`
    /// refuse a target outside it; `package lint` refuses a binary outside
    /// it. Not content: a range-only change moves no hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orion: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub connectors: Vec<String>,
    /// Plugins the workflows call that the artifact does not carry, by id
    /// and digest: `plan` checks the target serves exactly that digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginRequirement>,
    /// Models the workflows name by literal id that the artifact does not
    /// carry: `plan` checks the target serves each one active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelRequirement>,
    /// The `storage` connectors the carried models' references name and the
    /// artifact does not carry: the target fetches every artifact through
    /// one, and an import of a model whose connector is missing fails at
    /// write, so `plan` and `apply` check they exist first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<String>,
}

/// The importable content of one `models[]` entry — the projection the
/// server hashes a stored row with (`model_content`), computed here from the
/// import item: the manifest as the server will store it (validated, so a
/// defaulted `format` hashes the same whether or not it was spelled), the
/// reference without its advisory `size`, and the tags.
pub fn model_import_content(entry: &Value) -> Result<Value, Error> {
    let id = entry["model_id"].as_str().unwrap_or("?");
    let manifest = crate::model::Manifest::validated(&entry["manifest"]).map_err(|errors| {
        format!(
            "model entry '{id}': {}",
            errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let artifact = &entry["artifact"];
    for field in ["connector", "key", "digest"] {
        if artifact[field].as_str().is_none_or(|v| v.trim().is_empty()) {
            return Err(format!(
                "model entry '{}': artifact.{field} is required",
                manifest.name
            )
            .into());
        }
    }
    let tags: Vec<String> = entry["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    Ok(content::model_request_content(
        &serde_json::to_value(&manifest)?,
        artifact,
        &tags,
    ))
}

/// A `models[]` entry as the set loader sees it: the parsed manifest, with
/// no file behind it.
pub fn model_definition(
    index: usize,
    entry: &Value,
) -> Result<crate::definitions::ModelDefinition, Error> {
    let content = model_import_content(entry)?;
    let manifest: crate::model::Manifest = serde_json::from_value(content["manifest"].clone())?;
    Ok(crate::definitions::ModelDefinition::from_manifest(
        format!("models[{index}]"),
        manifest,
    ))
}

/// The importable content of one `plugins[]` entry — the projection the
/// server hashes a stored row with (`plugin_content`), computed here from the
/// import item: the manifest (TOML text or the object), the digest it names
/// or the hash of the component it carries, and its tags.
pub fn plugin_import_content(entry: &Value) -> Result<Value, Error> {
    let req: CreatePluginRequest = serde_json::from_value(entry.clone())
        .map_err(|e| format!("plugin entry does not parse as an import item: {e}"))?;
    let manifest = match &req.manifest {
        Value::String(text) => crate::plugin::Manifest::parse(text),
        other => serde_json::from_value::<crate::plugin::Manifest>(other.clone())
            .map_err(|e| {
                vec![crate::errors::FieldError::new(
                    "manifest",
                    "INVALID",
                    e.to_string(),
                )]
            })
            .and_then(crate::plugin::Manifest::validated),
    }
    .map_err(|errors| {
        format!(
            "plugin entry '{}': {}",
            req.plugin_id.as_deref().unwrap_or("?"),
            errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let digest = match (&req.digest, &req.component) {
        (Some(digest), _) => digest.clone(),
        (None, Some(component)) => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(component.trim())
                .map_err(|e| format!("plugin '{}': component is not base64: {e}", manifest.name))?;
            crate::plugin::WasmRuntime::digest(&bytes)
        }
        (None, None) => {
            return Err(format!(
                "plugin '{}': the entry carries neither a component nor a digest",
                manifest.name
            )
            .into());
        }
    };
    Ok(content::plugin_request_content(
        &serde_json::to_value(&manifest)?,
        &digest,
        &req.tags,
    ))
}

/// A `plugins[]` entry as the set loader sees it: the parsed manifest and
/// the digest the entry names or carries.
pub fn plugin_definition(
    index: usize,
    entry: &Value,
) -> Result<crate::definitions::PluginDefinition, Error> {
    let content = plugin_import_content(entry)?;
    let manifest: crate::plugin::Manifest = serde_json::from_value(content["manifest"].clone())?;
    Ok(crate::definitions::PluginDefinition {
        origin: format!("plugins[{index}]"),
        manifest,
        digest: content["digest"].as_str().map(str::to_string),
        component_path: None,
    })
}

/// Project every entry of one entity array through its import shape. Fails on
/// an entry that does not parse as that shape — such an artifact could not
/// apply anyway.
fn project_entries<T: serde::de::DeserializeOwned>(
    entries: &[Value],
    label: &str,
    project: impl Fn(&T) -> Value,
) -> Result<Vec<Value>, Error> {
    entries
        .iter()
        .map(|entry| {
            let req: T = serde_json::from_value(entry.clone())
                .map_err(|e| format!("{label} entry does not parse as an import item: {e}"))?;
            Ok(project(&req))
        })
        .collect()
}

/// The package-level hash: each entity array projected entry-by-entry
/// through the shared importable-content canonicalization, then hashed as
/// one document.
pub fn artifact_content_hash(artifact: &PackageArtifact) -> Result<String, Error> {
    let mut doc = json!({
        "connectors": project_entries::<CreateConnectorRequest>(
            &artifact.connectors, "connector", content::connector_request_content)?,
        "workflows": project_entries::<CreateWorkflowRequest>(
            &artifact.workflows, "workflow", content::workflow_request_content)?,
        "channels": project_entries::<CreateChannelRequest>(
            &artifact.channels, "channel", content::channel_request_content)?,
    });
    // The key is present only when there is something under it: a package
    // without plugins must hash exactly as it did before the member existed,
    // or every applied receipt on every target would read as a conflict.
    if !artifact.plugins.is_empty() {
        doc["plugins"] = Value::Array(
            artifact
                .plugins
                .iter()
                .map(plugin_import_content)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    // Models the same way, for the same reason.
    if !artifact.models.is_empty() {
        doc["models"] = Value::Array(
            artifact
                .models
                .iter()
                .map(model_import_content)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(content::content_hash(&doc))
}

pub fn read_artifact(path: &str) -> Result<PackageArtifact, Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    let artifact: PackageArtifact = serde_json::from_str(&raw)
        .map_err(|e| format!("'{path}' is not a package artifact: {e}"))?;
    Ok(artifact)
}

/// The member arrays in the order `apply` stages them — each after what it
/// references: plugins before the workflows that call them, connectors
/// before the models fetched through them and the workflows that use them,
/// workflows before the channels that name them.
pub fn members(artifact: &PackageArtifact) -> [(&'static str, &Vec<Value>); 5] {
    [
        ("plugins", &artifact.plugins),
        ("connectors", &artifact.connectors),
        ("models", &artifact.models),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ]
}

/// `N connectors, N workflows, N channels`, with the plugin and model
/// counts in front only when the artifact carries any — the pre-plugin
/// line otherwise.
pub fn member_counts(artifact: &PackageArtifact) -> String {
    let mut line = format!(
        "{} connectors, {} workflows, {} channels",
        artifact.connectors.len(),
        artifact.workflows.len(),
        artifact.channels.len(),
    );
    if !artifact.models.is_empty() {
        line = format!("{} models, {line}", artifact.models.len());
    }
    if !artifact.plugins.is_empty() {
        line = format!("{} plugins, {line}", artifact.plugins.len());
    }
    line
}

/// The artifact's `requires.orion`, parsed — `None` when it declares none.
pub fn declared_range(
    artifact: &PackageArtifact,
) -> Result<Option<crate::version::OrionRequirement>, Error> {
    artifact
        .requires
        .orion
        .as_deref()
        .map(|range| {
            crate::version::OrionRequirement::parse(range)
                .map_err(|e| format!("requires.orion {e}").into())
        })
        .transpose()
}

pub fn verify_hash(artifact: &PackageArtifact) -> Result<(), Error> {
    let actual = artifact_content_hash(artifact)?;
    if actual != artifact.package.content_hash {
        return Err(format!(
            "package.content_hash does not match the entities (expected {actual}) — \
             re-run `package lint` after editing an artifact"
        )
        .into());
    }
    Ok(())
}

/// `(kind, id, rollout)` of every entity the artifact marks `activate: true`,
/// in dependency order: workflows before the channels that name them. The one
/// place the intent fields are read, so plan and apply cannot disagree on
/// their spelling.
pub fn activation_intents(artifact: &PackageArtifact) -> Vec<(&'static str, String, Option<i64>)> {
    let mut intents = Vec::new();
    // Plugins first: a workflow's activation gate needs every plugin
    // function it names to be dispatchable, and only an active plugin is.
    for entry in &artifact.plugins {
        if entry["activate"] == true
            && let Some(id) = entry["plugin_id"].as_str()
        {
            intents.push(("plugins", id.to_string(), None));
        }
    }
    // Models next: a workflow naming one by literal id is quarantined on
    // the reload that follows unless the model is active by then.
    for entry in &artifact.models {
        if entry["activate"] == true
            && let Some(id) = entry["model_id"].as_str()
        {
            intents.push(("models", id.to_string(), None));
        }
    }
    for entry in &artifact.workflows {
        if entry["activate"] == true
            && let Some(id) = entry["workflow_id"].as_str()
        {
            intents.push((
                "workflows",
                id.to_string(),
                entry["rollout_percentage"].as_i64(),
            ));
        }
    }
    for entry in &artifact.channels {
        if entry["activate"] == true
            && let Some(id) = entry["channel_id"].as_str()
        {
            intents.push(("channels", id.to_string(), None));
        }
    }
    intents
}

/// The members an artifact carries, as the verification matches them.
pub fn package_members(artifact: &PackageArtifact) -> super::PackageMembers {
    super::PackageMembers::from_entries(
        &artifact.plugins,
        &artifact.models,
        &artifact.connectors,
        &artifact.workflows,
        &artifact.channels,
    )
}

/// The literal model ids a workflow entry's tasks name.
pub fn literal_model_ids(workflow: &Value) -> Vec<String> {
    workflow
        .get("tasks")
        .map(crate::model::literal_references)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, model)| model)
        .collect()
}

/// Fixtures the package modules' tests share.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn fixture_manifest() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/models/c4-tiny/model.json"
        ))
        .expect("fixture manifest")
    }

    pub(crate) fn model_entry() -> Value {
        json!({
            "model_id": "ada.c4-tiny",
            "manifest": fixture_manifest(),
            "artifact": {"connector": "models", "key": "c4/0.1.0.onnx", "digest": "sha256:abc"},
            "tags": ["fixture"],
            "activate": true,
        })
    }

    pub(crate) fn artifact(models: Vec<Value>) -> PackageArtifact {
        PackageArtifact {
            package: PackageMeta {
                name: "p".to_string(),
                version: "1.0.0".to_string(),
                orion: String::new(),
                content_hash: String::new(),
                exported_from: String::new(),
                exported_at: String::new(),
            },
            requires: Requires::default(),
            plugins: Vec::new(),
            models,
            connectors: Vec::new(),
            workflows: vec![json!({"workflow_id": "w", "name": "w", "tasks": []})],
            channels: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    /// The member is absent from the document and the hash when empty, so
    /// every receipt applied before models existed stays valid; present, it
    /// moves the hash and hashes as the server hashes the stored row —
    /// manifest, reference without its size, tags — and nothing else.
    #[test]
    fn the_models_member_is_omitted_when_empty_and_hashed_when_not() {
        let without = artifact(Vec::new());
        let rendered = serde_json::to_value(&without).expect("serialises");
        assert!(rendered.get("models").is_none(), "{rendered}");
        assert!(rendered["requires"].get("models").is_none());
        assert!(rendered["requires"].get("storage").is_none());
        let empty_hash = artifact_content_hash(&without).expect("hashes");

        let with = artifact(vec![model_entry()]);
        let rendered = serde_json::to_value(&with).expect("serialises");
        assert_eq!(rendered["models"][0]["model_id"], "ada.c4-tiny");
        let hash = artifact_content_hash(&with).expect("hashes");
        assert_ne!(hash, empty_hash, "a carried model is content");

        // The advisory size, the activation intent and the row-owned fields
        // an export carries are not content.
        let mut noisy = model_entry();
        noisy["artifact"]["size"] = json!(6171);
        noisy["status"] = json!("active");
        noisy["version"] = json!(3);
        noisy["admission"] = json!({"state": "passed"});
        noisy["activate"] = json!(false);
        assert_eq!(
            artifact_content_hash(&artifact(vec![noisy])).expect("hashes"),
            hash
        );
        // A manifest spelling its default `format` hashes as one that does
        // not: the server stores the validated form either way.
        let mut spelled = model_entry();
        spelled["manifest"]
            .as_object_mut()
            .expect("object")
            .remove("format");
        assert_eq!(
            artifact_content_hash(&artifact(vec![spelled])).expect("hashes"),
            hash
        );
        // The entry's projection is the row's: what `diff` compares.
        let projected = model_import_content(&model_entry()).expect("projects");
        assert_eq!(projected["artifact"]["digest"], "sha256:abc");
        assert!(projected["artifact"].get("size").is_none());
        assert_eq!(projected["tags"], json!(["fixture"]));
        assert_eq!(
            member_counts(&with),
            "1 models, 0 connectors, 1 workflows, 0 channels"
        );

        // An entry that is not an import item cannot be hashed — such an
        // artifact could not apply either.
        let mut broken = model_entry();
        broken["artifact"]["digest"] = json!("");
        let err = artifact_content_hash(&artifact(vec![broken])).expect_err("refused");
        assert!(err.to_string().contains("artifact.digest"), "{err}");
    }
}
