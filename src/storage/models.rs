use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::OrionError;

/// Parse a JSON string field, wrapping errors with entity context.
fn parse_json_field<T: serde::de::DeserializeOwned>(
    json_str: &str,
    entity_type: &str,
    entity_id: &str,
    field_name: &str,
) -> Result<T, OrionError> {
    serde_json::from_str(json_str).map_err(|e| OrionError::InternalSource {
        context: format!("Corrupt JSON in {entity_type} {entity_id} {field_name}"),
        source: Box::new(e),
    })
}

// -- Entity status enum --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EntityStatus {
    Draft,
    Active,
    Archived,
}

impl EntityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

impl std::fmt::Display for EntityStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parsed status-change action for type-safe exhaustive matching in handlers.
#[derive(Debug)]
pub enum StatusAction {
    Activate,
    Archive,
}

impl StatusAction {
    pub fn parse(status: EntityStatus) -> Result<Self, OrionError> {
        match status {
            EntityStatus::Active => Ok(Self::Activate),
            EntityStatus::Archived => Ok(Self::Archive),
            EntityStatus::Draft => Err(OrionError::BadRequest(
                "Invalid status transition to 'draft'. Use 'active' or 'archived'".to_string(),
            )),
        }
    }
}

// -- Channel type constants --
pub const CHANNEL_TYPE_SYNC: &str = "sync";
pub const CHANNEL_TYPE_ASYNC: &str = "async";
pub const VALID_CHANNEL_TYPES: [&str; 2] = [CHANNEL_TYPE_SYNC, CHANNEL_TYPE_ASYNC];

// -- Channel protocol enum --
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelProtocol {
    Rest,
    Http,
    Kafka,
}

impl ChannelProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Http => "http",
            Self::Kafka => "kafka",
        }
    }
}

impl std::fmt::Display for ChannelProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// -- Trace status constants --
pub const TRACE_STATUS_PENDING: &str = "pending";
pub const TRACE_STATUS_RUNNING: &str = "running";
pub const TRACE_STATUS_COMPLETED: &str = "completed";
pub const TRACE_STATUS_FAILED: &str = "failed";

// -- Trace mode constants --
pub const TRACE_MODE_SYNC: &str = "sync";
pub const TRACE_MODE_ASYNC: &str = "async";

// ============================================================
// Workflow (replaces Rule)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Workflow {
    pub workflow_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub priority: i64,
    pub status: String,
    pub rollout_percentage: i64,
    pub condition_json: String,
    pub tasks_json: String,
    pub tags: String,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// API-friendly representation of a Workflow with parsed JSON fields.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowResponse {
    pub workflow_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub priority: i64,
    pub status: String,
    pub rollout_percentage: i64,
    pub condition: Value,
    pub tasks: Value,
    pub tags: Value,
    pub continue_on_error: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl TryFrom<&Workflow> for WorkflowResponse {
    type Error = OrionError;

    fn try_from(workflow: &Workflow) -> Result<Self, Self::Error> {
        let id = &workflow.workflow_id;
        Ok(Self {
            workflow_id: workflow.workflow_id.clone(),
            version: workflow.version,
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            priority: workflow.priority,
            status: workflow.status.clone(),
            rollout_percentage: workflow.rollout_percentage,
            condition: parse_json_field(
                &workflow.condition_json,
                "workflow",
                id,
                "condition_json",
            )?,
            tasks: parse_json_field(&workflow.tasks_json, "workflow", id, "tasks_json")?,
            tags: parse_json_field(&workflow.tags, "workflow", id, "tags")?,
            continue_on_error: workflow.continue_on_error,
            created_at: workflow.created_at,
            updated_at: workflow.updated_at,
        })
    }
}

// ============================================================
// Channel
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Channel {
    pub channel_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: String,
    pub protocol: String,
    pub methods: Option<String>,
    pub route_pattern: Option<String>,
    pub topic: Option<String>,
    pub consumer_group: Option<String>,
    pub transport_config_json: String,
    pub workflow_id: Option<String>,
    pub config_json: String,
    pub status: String,
    pub priority: i64,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// API-friendly representation of a Channel with parsed JSON fields.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelResponse {
    pub channel_id: String,
    pub version: i64,
    pub name: String,
    pub description: Option<String>,
    pub channel_type: String,
    pub protocol: String,
    pub methods: Option<Value>,
    pub route_pattern: Option<String>,
    pub topic: Option<String>,
    pub consumer_group: Option<String>,
    pub transport_config: Value,
    pub workflow_id: Option<String>,
    pub config: Value,
    pub status: String,
    pub priority: i64,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl TryFrom<&Channel> for ChannelResponse {
    type Error = OrionError;

    fn try_from(channel: &Channel) -> Result<Self, Self::Error> {
        let id = &channel.channel_id;
        let methods = channel
            .methods
            .as_ref()
            .map(|m| parse_json_field(m, "channel", id, "methods"))
            .transpose()?;

        Ok(Self {
            channel_id: channel.channel_id.clone(),
            version: channel.version,
            name: channel.name.clone(),
            description: channel.description.clone(),
            channel_type: channel.channel_type.clone(),
            protocol: channel.protocol.clone(),
            methods,
            route_pattern: channel.route_pattern.clone(),
            topic: channel.topic.clone(),
            consumer_group: channel.consumer_group.clone(),
            transport_config: parse_json_field(
                &channel.transport_config_json,
                "channel",
                id,
                "transport_config_json",
            )?,
            workflow_id: channel.workflow_id.clone(),
            config: parse_json_field(&channel.config_json, "channel", id, "config_json")?,
            status: channel.status.clone(),
            priority: channel.priority,
            created_at: channel.created_at,
            updated_at: channel.updated_at,
        })
    }
}

// ============================================================
// Connector (unchanged)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub connector_type: String,
    pub config_json: String,
    pub enabled: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// ============================================================
// Trace
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Trace {
    pub id: String,
    pub channel: String,
    pub channel_id: Option<String>,
    pub mode: String,
    pub status: String,
    pub input_json: Option<String>,
    pub result_json: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: Option<f64>,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// -- Trace DLQ model --

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TraceDlqEntry {
    pub id: String,
    pub trace_id: String,
    pub channel: String,
    pub payload_json: String,
    pub metadata_json: String,
    pub error_message: String,
    pub retry_count: i64,
    pub max_retries: i64,
    pub next_retry_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditLogEntry {
    pub id: String,
    pub principal: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub details: Option<String>,
    pub created_at: NaiveDateTime,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn sample_workflow() -> Workflow {
        Workflow {
            workflow_id: "wf-1".to_string(),
            name: "Test Workflow".to_string(),
            description: Some("A test workflow".to_string()),
            priority: 10,
            version: 1,
            status: EntityStatus::Active.as_str().to_string(),
            rollout_percentage: 100,
            condition_json: r#"{"==": [1, 1]}"#.to_string(),
            tasks_json: r#"[{"id": "t1", "function": "http_call"}]"#.to_string(),
            tags: r#"["test"]"#.to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        }
    }

    fn sample_channel() -> Channel {
        Channel {
            channel_id: "ch-1".to_string(),
            version: 1,
            name: "orders".to_string(),
            description: Some("Order processing channel".to_string()),
            channel_type: CHANNEL_TYPE_SYNC.to_string(),
            protocol: ChannelProtocol::Rest.as_str().to_string(),
            methods: Some(r#"["POST"]"#.to_string()),
            route_pattern: Some("/orders".to_string()),
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: Some("wf-1".to_string()),
            config_json: r#"{"timeout_ms": 5000}"#.to_string(),
            status: EntityStatus::Active.as_str().to_string(),
            priority: 0,
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        }
    }

    #[test]
    fn test_workflow_response_try_from_valid() {
        let workflow = sample_workflow();
        let response = WorkflowResponse::try_from(&workflow).unwrap();
        assert_eq!(response.workflow_id, "wf-1");
        assert_eq!(response.name, "Test Workflow");
        assert_eq!(response.priority, 10);
        assert_eq!(response.version, 1);
        assert_eq!(response.status, EntityStatus::Active.as_str());
        assert_eq!(response.rollout_percentage, 100);
        assert_eq!(response.condition, serde_json::json!({"==": [1, 1]}));
        assert_eq!(
            response.tasks,
            serde_json::json!([{"id": "t1", "function": "http_call"}])
        );
        assert_eq!(response.tags, serde_json::json!(["test"]));
        assert!(!response.continue_on_error);
    }

    #[test]
    fn test_workflow_response_try_from_invalid_condition_json() {
        let mut workflow = sample_workflow();
        workflow.condition_json = "not valid json {{{".to_string();
        let result = WorkflowResponse::try_from(&workflow);
        assert!(result.is_err());
    }

    #[test]
    fn test_workflow_response_try_from_invalid_tasks_json() {
        let mut workflow = sample_workflow();
        workflow.tasks_json = "invalid".to_string();
        let result = WorkflowResponse::try_from(&workflow);
        assert!(result.is_err());
    }

    #[test]
    fn test_workflow_response_try_from_invalid_tags_json() {
        let mut workflow = sample_workflow();
        workflow.tags = "not json".to_string();
        let result = WorkflowResponse::try_from(&workflow);
        assert!(result.is_err());
    }

    #[test]
    fn test_workflow_response_try_from_no_description() {
        let mut workflow = sample_workflow();
        workflow.description = None;
        let response = WorkflowResponse::try_from(&workflow).unwrap();
        assert!(response.description.is_none());
    }

    #[test]
    fn test_channel_response_try_from_valid() {
        let channel = sample_channel();
        let response = ChannelResponse::try_from(&channel).unwrap();
        assert_eq!(response.channel_id, "ch-1");
        assert_eq!(response.name, "orders");
        assert_eq!(response.channel_type, CHANNEL_TYPE_SYNC);
        assert_eq!(response.protocol, ChannelProtocol::Rest.as_str());
        assert_eq!(response.methods, Some(serde_json::json!(["POST"])));
        assert_eq!(response.route_pattern, Some("/orders".to_string()));
        assert!(response.topic.is_none());
        assert_eq!(response.workflow_id, Some("wf-1".to_string()));
        assert_eq!(response.config, serde_json::json!({"timeout_ms": 5000}));
    }

    #[test]
    fn test_channel_response_try_from_async() {
        let mut channel = sample_channel();
        channel.channel_type = CHANNEL_TYPE_ASYNC.to_string();
        channel.protocol = ChannelProtocol::Kafka.as_str().to_string();
        channel.methods = None;
        channel.route_pattern = None;
        channel.topic = Some("order.placed".to_string());
        channel.consumer_group = Some("orion".to_string());
        let response = ChannelResponse::try_from(&channel).unwrap();
        assert_eq!(response.channel_type, CHANNEL_TYPE_ASYNC);
        assert_eq!(response.protocol, ChannelProtocol::Kafka.as_str());
        assert!(response.methods.is_none());
        assert_eq!(response.topic, Some("order.placed".to_string()));
    }

    #[test]
    fn test_channel_response_try_from_invalid_config_json() {
        let mut channel = sample_channel();
        channel.config_json = "bad json".to_string();
        let result = ChannelResponse::try_from(&channel);
        assert!(result.is_err());
    }

    #[test]
    fn test_entity_status_as_str() {
        assert_eq!(EntityStatus::Draft.as_str(), "draft");
        assert_eq!(EntityStatus::Active.as_str(), "active");
        assert_eq!(EntityStatus::Archived.as_str(), "archived");
    }

    #[test]
    fn test_entity_status_display() {
        assert_eq!(EntityStatus::Draft.to_string(), "draft");
        assert_eq!(EntityStatus::Active.to_string(), "active");
        assert_eq!(EntityStatus::Archived.to_string(), "archived");
    }

    #[test]
    fn test_entity_status_serde_roundtrip() {
        let draft: EntityStatus = serde_json::from_str(r#""draft""#).unwrap();
        assert_eq!(draft, EntityStatus::Draft);
        let active: EntityStatus = serde_json::from_str(r#""active""#).unwrap();
        assert_eq!(active, EntityStatus::Active);
        let archived: EntityStatus = serde_json::from_str(r#""archived""#).unwrap();
        assert_eq!(archived, EntityStatus::Archived);
        // Invalid status should fail
        assert!(serde_json::from_str::<EntityStatus>(r#""pending""#).is_err());
    }

    #[test]
    fn test_valid_channel_types() {
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_SYNC));
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_ASYNC));
    }

    #[test]
    fn test_channel_protocol_as_str() {
        assert_eq!(ChannelProtocol::Rest.as_str(), "rest");
        assert_eq!(ChannelProtocol::Http.as_str(), "http");
        assert_eq!(ChannelProtocol::Kafka.as_str(), "kafka");
    }

    #[test]
    fn test_channel_protocol_display() {
        assert_eq!(ChannelProtocol::Rest.to_string(), "rest");
        assert_eq!(ChannelProtocol::Http.to_string(), "http");
        assert_eq!(ChannelProtocol::Kafka.to_string(), "kafka");
    }

    #[test]
    fn test_channel_protocol_serde_roundtrip() {
        let rest: ChannelProtocol = serde_json::from_str(r#""rest""#).unwrap();
        assert_eq!(rest, ChannelProtocol::Rest);
        let http: ChannelProtocol = serde_json::from_str(r#""http""#).unwrap();
        assert_eq!(http, ChannelProtocol::Http);
        let kafka: ChannelProtocol = serde_json::from_str(r#""kafka""#).unwrap();
        assert_eq!(kafka, ChannelProtocol::Kafka);
        // Invalid protocol should fail
        assert!(serde_json::from_str::<ChannelProtocol>(r#""grpc""#).is_err());
    }

    #[test]
    fn test_trace_status_constants() {
        assert_eq!(TRACE_STATUS_PENDING, "pending");
        assert_eq!(TRACE_STATUS_RUNNING, "running");
        assert_eq!(TRACE_STATUS_COMPLETED, "completed");
        assert_eq!(TRACE_STATUS_FAILED, "failed");
    }

    #[test]
    fn test_trace_mode_constants() {
        assert_eq!(TRACE_MODE_SYNC, "sync");
        assert_eq!(TRACE_MODE_ASYNC, "async");
    }
}
