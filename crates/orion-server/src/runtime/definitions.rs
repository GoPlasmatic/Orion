//! Deployable definitions: their canonical bytes, and the internal rows the
//! generation builder reads.
//!
//! A manifest member names a definition by digest, and the artifact store
//! holds it as the canonical JSON of an [`orion_api::v2::Definition`] — kind
//! and document together, so the digest binds the kind. Canonical means the
//! typed document re-serialised (every defaulted field present) and then put
//! in canonical key order, so two uploads of the same definition, however
//! they were spelled, are one artifact.
//!
//! The generation builder still consumes `Channel`, `Workflow` and `Plugin`
//! rows; a definition becomes one here, with the identity the member gives
//! it. Nothing about a row comes from anywhere but the definition and the
//! member.

use base64::Engine as _;
use orion_api::v2::{
    ChannelDefinition, Definition, PluginDefinition, WorkflowDefinition, WorkflowOnFailure,
};

use crate::errors::OrionError;
use crate::storage::models::{Channel, EntityStatus, Plugin, Workflow};

/// The bytes the artifact store holds for `definition`: keys sorted at every
/// depth, compact. Not the package-manifest encoder, which refuses booleans,
/// floats and non-ASCII text that a workflow is made of.
pub fn canonical_bytes(definition: &Definition) -> Result<Vec<u8>, OrionError> {
    let value = serde_json::to_value(definition)?;
    Ok(crate::canonical::canonical_json(&value).into_bytes())
}

/// The definition stored as `bytes`, read strictly.
pub fn parse(bytes: &[u8]) -> Result<Definition, OrionError> {
    serde_json::from_slice(bytes)
        .map_err(|e| OrionError::validation(format!("stored definition does not parse: {e}")))
}

/// A workflow definition as the row the builder reads, `revision` its
/// version. Active by construction: a deployed member serves.
pub fn workflow_row(
    definition: &WorkflowDefinition,
    workflow_id: &str,
    revision: u64,
) -> Result<Workflow, OrionError> {
    let now = chrono::Utc::now().naive_utc();
    Ok(Workflow {
        workflow_id: workflow_id.to_string(),
        version: version(revision),
        name: definition.name.clone(),
        description: definition.description.clone(),
        priority: definition.priority,
        status: EntityStatus::Active.as_str().to_string(),
        // The manifest's rollout intervals decide the split; a lone revision
        // takes every bucket.
        rollout_percentage: 100,
        condition_json: serde_json::to_string(&definition.condition)?,
        tasks_json: serde_json::to_string(&definition.tasks)?,
        tags_json: serde_json::to_string(&definition.tags)?,
        loop_json: definition
            .loop_config
            .as_ref()
            .filter(|l| !l.is_null())
            .map(serde_json::to_string)
            .transpose()?,
        continue_on_error: definition.on_failure == WorkflowOnFailure::Continue,
        created_at: now,
        updated_at: now,
    })
}

/// A channel definition as the row the builder reads, bound to
/// `workflow_id` (the Workflow its member requires).
pub fn channel_row(
    definition: &ChannelDefinition,
    channel_id: &str,
    revision: u64,
    workflow_id: Option<String>,
) -> Result<Channel, OrionError> {
    let now = chrono::Utc::now().naive_utc();
    Ok(Channel {
        channel_id: channel_id.to_string(),
        version: version(revision),
        name: definition.name.clone(),
        description: definition.description.clone(),
        channel_type: definition.channel_type.as_str().to_string(),
        protocol: definition.protocol.as_str().to_string(),
        methods_json: definition
            .methods
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        route_pattern: definition.route_pattern.clone(),
        topic: None,
        consumer_group: None,
        transport_config_json: serde_json::to_string(&definition.transport_config)?,
        workflow_id,
        config_json: serde_json::to_string(&definition.config)?,
        status: EntityStatus::Active.as_str().to_string(),
        priority: definition.priority,
        tags_json: serde_json::to_string(&definition.tags)?,
        created_at: now,
        updated_at: now,
    })
}

/// A plugin definition as the row the builder reads, and its component.
pub fn plugin_row(
    definition: &PluginDefinition,
    plugin_id: &str,
    revision: u64,
) -> Result<(Plugin, Vec<u8>), OrionError> {
    let bytes = component_bytes(definition)?;
    let now = chrono::Utc::now().naive_utc();
    Ok((
        Plugin {
            plugin_id: plugin_id.to_string(),
            version: version(revision),
            status: EntityStatus::Active.as_str().to_string(),
            digest: crate::crypto::sha256_digest(&bytes),
            manifest_json: serde_json::to_string(&definition.manifest)?,
            tags_json: serde_json::to_string(&definition.tags)?,
            signature: definition.signature.clone(),
            created_at: now,
            updated_at: now,
        },
        bytes,
    ))
}

/// The component a plugin definition carries, decoded.
pub fn component_bytes(definition: &PluginDefinition) -> Result<Vec<u8>, OrionError> {
    base64::engine::general_purpose::STANDARD
        .decode(definition.component.trim())
        .map_err(|e| {
            OrionError::invalid_field(
                "definition.component",
                "INVALID",
                format!("component is not base64: {e}"),
            )
        })
}

fn version(revision: u64) -> i64 {
    i64::try_from(revision).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workflow(extra: serde_json::Value) -> Definition {
        let mut doc = json!({
            "kind": "workflow",
            "definition": {"name": "w", "tasks": []},
        });
        if let (Some(def), Some(extra)) = (doc["definition"].as_object_mut(), extra.as_object()) {
            def.extend(extra.clone());
        }
        serde_json::from_value(doc).expect("a workflow definition")
    }

    /// Two spellings of one definition — a defaulted field written out or
    /// left off, keys in another order — are one artifact.
    #[test]
    fn a_definition_has_one_canonical_form() {
        let terse = workflow(json!({}));
        let spelled: Definition = serde_json::from_value(json!({
            "definition": {
                "tasks": [], "on_failure": "fail", "condition": true,
                "priority": 0, "name": "w", "tags": []
            },
            "kind": "workflow",
        }))
        .expect("parses");
        assert_eq!(
            canonical_bytes(&terse).expect("canonical"),
            canonical_bytes(&spelled).expect("canonical")
        );
        let back = parse(&canonical_bytes(&terse).expect("canonical")).expect("parses back");
        assert_eq!(back, terse);
    }

    /// A key the kind does not define is refused, as is an unknown kind: a
    /// definition is read strictly, like every other document.
    #[test]
    fn a_definition_is_read_strictly() {
        let unknown_key = serde_json::from_value::<Definition>(json!({
            "kind": "workflow", "definition": {"name": "w", "tasks": [], "continue_on_error": true}
        }));
        assert!(
            unknown_key.is_err(),
            "the v1 field is not a definition field"
        );
        let unknown_kind = serde_json::from_value::<Definition>(json!({
            "kind": "model", "definition": {}
        }));
        assert!(unknown_kind.is_err());
    }

    #[test]
    fn a_workflow_row_takes_its_identity_from_the_member() {
        let Definition::Workflow(def) = workflow(json!({"on_failure": "continue"})) else {
            unreachable!()
        };
        let row = workflow_row(&def, "orders", 3).expect("a row");
        assert_eq!((row.workflow_id.as_str(), row.version), ("orders", 3));
        assert!(row.continue_on_error);
        assert_eq!(row.status, "active");
    }
}
