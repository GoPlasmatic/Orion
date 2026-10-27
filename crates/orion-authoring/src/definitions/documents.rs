//! The documents a definition set is written in: one workflow, channel or
//! connector per file, in the shape of a v1 admin API create body.
//! `lint <dir>`, `compile` and the offline commands read them; `compile`
//! turns them into v2 definitions and connections.

use serde::Deserialize;
use serde_json::Value;

use crate::storage::models::{ChannelType, Workflow};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateWorkflowRequest {
    pub workflow_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_condition")]
    pub condition: Value,
    pub tasks: Value,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Engine-managed loop over `tasks`, spelled `loop` on the wire to match
    /// dataflow-rs. Absent — the overwhelmingly common case — runs the task
    /// list exactly once.
    #[serde(default, rename = "loop")]
    pub loop_config: Option<serde_json::Value>,
    #[serde(default)]
    pub continue_on_error: bool,
}

fn default_condition() -> serde_json::Value {
    serde_json::Value::Bool(true)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateChannelRequest {
    pub channel_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: ChannelType,
    #[serde(deserialize_with = "orion_api::enums::deserialize_protocol")]
    pub protocol: String,
    pub methods: Option<Vec<String>>,
    pub route_pattern: Option<String>,
    pub topic: Option<String>,
    pub consumer_group: Option<String>,
    #[serde(default = "default_empty_object")]
    pub transport_config: Value,
    pub workflow_id: Option<String>,
    #[serde(default = "default_empty_object")]
    pub config: Value,
    #[serde(default)]
    pub priority: i64,
    /// Selection labels, same contract as workflow tags — filter with
    /// `?tag=` on list and export.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateConnectorRequest {
    pub id: Option<String>,
    pub name: String,
    pub connector_type: crate::connector::ConnectorType,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
    /// Whether the connector loads into the registry. Defaults to true.
    ///
    /// `/export` has always emitted this field; until K1 the create path
    /// silently dropped it, so a *disabled* connector promoted through
    /// export → import came back **enabled** in the target environment.
    pub enabled: Option<bool>,
    /// Selection labels, same contract as workflow tags — filter with
    /// `?tag=` on list and export.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_config() -> serde_json::Value {
    serde_json::json!({})
}

/// Build an in-memory, never-persisted `Workflow` (active, version 1, 100%
/// rollout) from a definition-set workflow. For the offline paths, which need
/// to build its engine JSON (`engine::workflow_json`) with no database.
pub fn synthetic_workflow(
    req: &CreateWorkflowRequest,
    id: &str,
) -> Result<Workflow, serde_json::Error> {
    let now = chrono::Utc::now().naive_utc();
    Ok(Workflow {
        workflow_id: id.to_string(),
        version: 1,
        name: req.name.clone(),
        description: req.description.clone(),
        priority: req.priority,
        status: crate::storage::models::EntityStatus::Active
            .as_str()
            .to_string(),
        rollout_percentage: 100,
        condition_json: serde_json::to_string(&req.condition)?,
        tasks_json: serde_json::to_string(&req.tasks)?,
        tags_json: serde_json::to_string(&req.tags)?,
        loop_json: match &req.loop_config {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(serde_json::to_string(v)?),
        },
        continue_on_error: req.continue_on_error,
        created_at: now,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_workflow_request_defaults() {
        let json = r#"{"name": "Test", "tasks": []}"#;
        let req: CreateWorkflowRequest = serde_json::from_str(json).expect("test");
        assert!(req.workflow_id.is_none());
        assert_eq!(req.name, "Test");
        assert_eq!(req.priority, 0);
        assert_eq!(req.condition, serde_json::Value::Bool(true));
        assert!(req.tags.is_empty());
        assert!(!req.continue_on_error);
    }
}
