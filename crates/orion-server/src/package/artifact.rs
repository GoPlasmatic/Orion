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
    Ok(content::content_hash(&doc))
}

pub fn read_artifact(path: &str) -> Result<PackageArtifact, Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    let doc: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("'{path}' is not a package artifact: {e}"))?;
    refuse_models(&doc).map_err(|e| format!("'{path}': {e}"))?;
    let artifact: PackageArtifact = serde_json::from_value(doc)
        .map_err(|e| format!("'{path}' is not a package artifact: {e}"))?;
    Ok(artifact)
}

/// An artifact compiled for an edition that serves ONNX models can carry a
/// `models` member and `requires.models` / `requires.storage`. This build
/// has no model runtime and no storage connector, and the fields are not
/// part of [`PackageArtifact`] — dropping them silently would apply a
/// package without members its workflows depend on, so they are refused by
/// name instead.
pub fn refuse_models(doc: &Value) -> Result<(), Error> {
    let non_empty = |v: &Value| v.as_array().is_some_and(|a| !a.is_empty());
    let carried = [
        ("models", &doc["models"]),
        ("requires.models", &doc["requires"]["models"]),
        ("requires.storage", &doc["requires"]["storage"]),
    ];
    match carried.iter().find(|(_, v)| non_empty(v)) {
        Some((field, _)) => Err(format!(
            "the artifact carries `{field}`, and this build serves no ONNX models or object \
             storage connectors"
        )
        .into()),
        None => Ok(()),
    }
}

/// The member arrays in the order `apply` stages them — each after what it
/// references: plugins before the workflows that call them, connectors
/// before the workflows that use them, workflows before the channels that
/// name them.
pub fn members(artifact: &PackageArtifact) -> [(&'static str, &Vec<Value>); 4] {
    [
        ("plugins", &artifact.plugins),
        ("connectors", &artifact.connectors),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ]
}

/// `N connectors, N workflows, N channels`, with the plugin count in front
/// only when the artifact carries any — the pre-plugin line otherwise.
pub fn member_counts(artifact: &PackageArtifact) -> String {
    let mut line = format!(
        "{} connectors, {} workflows, {} channels",
        artifact.connectors.len(),
        artifact.workflows.len(),
        artifact.channels.len(),
    );
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
        &artifact.connectors,
        &artifact.workflows,
        &artifact.channels,
    )
}

/// Fixtures the package modules' tests share.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn artifact() -> PackageArtifact {
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

    /// An artifact from an edition that serves models names them in
    /// `models`, `requires.models` or `requires.storage`; each is refused by
    /// name rather than dropped, and an artifact without them reads as before.
    #[test]
    fn a_model_bearing_artifact_is_refused_by_name() {
        let plain = serde_json::to_value(artifact()).expect("serialises");
        assert!(refuse_models(&plain).is_ok());
        let mut carried = plain.clone();
        carried["models"] = json!([{"model_id": "m"}]);
        let mut required = plain.clone();
        required["requires"]["models"] = json!([{"id": "m"}]);
        let mut storage = plain.clone();
        storage["requires"]["storage"] = json!(["s3"]);
        for (field, doc) in [
            ("models", carried),
            ("requires.models", required),
            ("requires.storage", storage),
        ] {
            let err = refuse_models(&doc).expect_err(field);
            assert!(err.to_string().contains(&format!("`{field}`")), "{err}");
        }
        // Empty arrays are what an older compile wrote for "none".
        let mut empty = plain.clone();
        empty["models"] = json!([]);
        empty["requires"]["storage"] = json!([]);
        assert!(refuse_models(&empty).is_ok());
    }
}
