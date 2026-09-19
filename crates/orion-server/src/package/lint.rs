//! `package lint`: an artifact judged offline — its envelope, its content
//! hash, the signatures it carries, and the cross-reference checks the
//! shared definition-set pass runs. No server, no secrets.

use serde_json::Value;

use super::Error;
use super::artifact::{PackageArtifact, artifact_content_hash, declared_range};

/// What linting one artifact found. `errors` fail the lint; `warnings` are
/// printed and do not.
#[derive(Debug, Default)]
pub struct LintReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Lint `artifact`. `Err` only when this binary is outside the artifact's
/// `requires.orion` range, said in one line rather than as the errors of
/// features the binary predates.
pub fn lint_artifact(artifact: &PackageArtifact) -> Result<LintReport, Error> {
    // This binary's validators are about to judge the entities: an artifact
    // declaring a range it is outside of says so first.
    if let Some(range) = declared_range(artifact)? {
        range.check_this_binary(&format!(
            "{}@{}",
            artifact.package.name, artifact.package.version
        ))?;
    }
    let mut errors: Vec<String> = Vec::new();

    if artifact.package.name.trim().is_empty() {
        errors.push("package.name is empty".to_string());
    }
    if artifact.package.version.trim().is_empty() {
        errors.push("package.version is empty".to_string());
    }

    // The hash is part of the contract: an artifact edited without
    // re-hashing would defeat the receipt comparison downstream.
    match artifact_content_hash(artifact) {
        Ok(actual) if actual != artifact.package.content_hash => errors.push(format!(
            "package.content_hash does not match the entities — expected {actual}"
        )),
        Ok(actual) => {
            // A `content-<12 hex>` version names the content it was derived
            // from; one that names other content was edited by hand.
            if let Some(hex) =
                crate::storage::content::content_version_hex(&artifact.package.version)
                && let Ok(expected) = crate::storage::content::content_version(&actual, None)
                && expected != artifact.package.version
            {
                errors.push(format!(
                    "package.version 'content-{hex}' names content the entities do not hash \
                     to — expected {expected}"
                ));
            }
        }
        Err(e) => errors.push(e.to_string()),
    }

    // A signature is not content, so the hash cannot vouch for it: a garbage
    // value would otherwise surface only when the target refuses the import.
    for (kind, entries) in [("plugins", &artifact.plugins), ("models", &artifact.models)] {
        for (index, entry) in entries.iter().enumerate() {
            if let Some(signature) = entry.get("signature").and_then(Value::as_str)
                && let Err(e) = crate::crypto::ed25519::normalize_signature(signature)
            {
                errors.push(format!("{kind}[{index}].signature: {e}"));
            }
        }
    }

    // Everything below the package envelope is a definition set, checked by
    // the shared pass. `requires` is this container's boundary: names the
    // target instance is expected to already have. The set's plugins are the
    // artifact's own entries, so a workflow naming one of their functions is
    // checked against the manifest that travels with it.
    let (set, boundary, mut findings) = artifact_as_set(artifact);
    let registry = match set.function_registry() {
        Ok(registry) => registry,
        Err(reason) => {
            errors.push(format!("plugins: {reason}"));
            crate::engine::FunctionRegistry::builtin()
                .with_entries(Vec::new())
                .expect("the built-in registry extends by nothing")
        }
    };
    findings.extend(crate::definitions::check(&set, &boundary, true, &registry));

    let warnings = findings
        .iter()
        .filter(|f| !f.is_error())
        .map(ToString::to_string)
        .collect();
    errors.extend(findings.iter().filter(|f| f.is_error()).map(|f| {
        // The package surface reports one flat line per problem; the
        // structured form is what `lint <dir>` renders.
        format!("{}: {}", f.entity, f.message)
    }));
    Ok(LintReport { errors, warnings })
}

/// Project an artifact into the shared
/// [`DefinitionSet`](crate::definitions::DefinitionSet) shape, keeping the
/// `channels[2]`-style origins the package surface has always reported.
///
/// The third member of the result is what the plugin entries could not
/// give the set: an entry that does not parse as an import item is a finding
/// here, in the same voice as an entity that does not.
pub fn artifact_as_set(
    artifact: &PackageArtifact,
) -> (
    crate::definitions::DefinitionSet,
    crate::definitions::Boundary,
    Vec<crate::definitions::Diagnostic>,
) {
    use crate::definitions::Entity;
    let mut entries = Vec::new();
    for (i, doc) in artifact.connectors.iter().enumerate() {
        entries.push((Entity::Connector, format!("connectors[{i}]"), doc.clone()));
    }
    for (i, doc) in artifact.workflows.iter().enumerate() {
        entries.push((Entity::Workflow, format!("workflows[{i}]"), doc.clone()));
    }
    for (i, doc) in artifact.channels.iter().enumerate() {
        entries.push((Entity::Channel, format!("channels[{i}]"), doc.clone()));
    }
    let boundary = crate::definitions::Boundary {
        channels: artifact.requires.channels.clone(),
        connectors: artifact.requires.connectors.clone(),
        models: artifact
            .requires
            .models
            .iter()
            .map(|m| m.id.clone())
            .collect(),
    };
    let mut set = crate::definitions::DefinitionSet::from_entries(entries);
    let mut findings = Vec::new();
    for (i, entry) in artifact.plugins.iter().enumerate() {
        match super::artifact::plugin_definition(i, entry) {
            Ok(plugin) => set.plugins.push(plugin),
            Err(e) => findings.push(crate::definitions::Diagnostic::error(
                "parse.plugin",
                format!("plugins[{i}]"),
                e.to_string(),
            )),
        }
    }
    for (i, entry) in artifact.models.iter().enumerate() {
        match super::artifact::model_definition(i, entry) {
            Ok(model) => set.models.push(model),
            Err(e) => findings.push(crate::definitions::Diagnostic::error(
                "parse.model",
                format!("models[{i}]"),
                e.to_string(),
            )),
        }
    }
    (set, boundary, findings)
}

#[cfg(test)]
mod tests {
    use super::super::artifact::test_support::*;
    use super::super::artifact::{
        ModelRequirement, activation_intents, literal_model_ids, members,
    };
    use super::*;
    use serde_json::json;

    /// `requires.models` and `requires.storage` round-trip, tolerate an
    /// artifact written before they existed, and make the model boundary
    /// the set check reads.
    #[test]
    fn requires_carries_models_and_storage_and_bounds_the_set() {
        let mut with = artifact(vec![model_entry()]);
        with.requires.models.push(ModelRequirement {
            id: "ada.other".to_string(),
            version: 2,
            digest: "sha256:def".to_string(),
        });
        with.requires.storage.push("models".to_string());
        with.workflows[0]["tasks"] = json!([
            {"id": "a", "name": "a", "function": {"name": "model_infer",
                "input": {"model": "ada.c4-tiny", "input": {"var": ""}}}},
            {"id": "b", "name": "b", "function": {"name": "model_infer",
                "input": {"model": "ada.other", "input": {"var": ""}}}}
        ]);
        let text = serde_json::to_string(&with).expect("serialises");
        let back: PackageArtifact = serde_json::from_str(&text).expect("parses");
        assert_eq!(back.requires.models, with.requires.models);
        assert_eq!(back.requires.storage, ["models"]);
        assert_eq!(
            literal_model_ids(&back.workflows[0]),
            ["ada.c4-tiny", "ada.other"]
        );

        let (set, boundary, findings) = artifact_as_set(&back);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(set.models.len(), 1);
        assert_eq!(set.models[0].origin, "models[0]");
        assert!(boundary.allows_model("ada.other"));
        assert!(
            !boundary.allows_model("ada.c4-tiny"),
            "carried, not required"
        );
        let registry = set.function_registry().expect("registry");
        let findings = crate::definitions::check(&set, &boundary, true, &registry);
        assert!(
            !findings.iter().any(|f| f.check == "closure.model"),
            "{findings:#?}"
        );

        // Written before the fields existed: both default to empty.
        let old: PackageArtifact = serde_json::from_value(json!({
            "package": {"name": "p", "version": "1", "content_hash": "x"},
            "requires": {"channels": [], "connectors": []},
        }))
        .expect("parses");
        assert!(old.requires.models.is_empty() && old.requires.storage.is_empty());
        assert!(old.models.is_empty());

        // The intents order models after plugins and before workflows.
        let intents = activation_intents(&with);
        assert_eq!(intents[0].0, "models");
        assert_eq!(intents[0].1, "ada.c4-tiny");
        assert_eq!(
            members(&with).map(|(k, _)| k),
            ["plugins", "connectors", "models", "workflows", "channels"]
        );
    }
}
