//! Row → DTO conversions: how a stored row becomes what the HTTP API serves.
//!
//! The DTO shapes themselves moved to the shared `orion-api` crate (they are
//! the wire contract, and the CLI deserializes them); they are re-exported
//! here under their pre-1.0 paths. What stays is every `From`/`TryFrom` from
//! a row in [`super::rows`] into a DTO. That conversion is the only door
//! between the database and a response body, which is what makes "which
//! columns are public?" answerable by reading one file (D27, D28).

use serde_json::Value;

use super::enums::parse_json_field;
use super::rows::{
    AuditLogEntry, Channel, Connector, PackageReceipt, TraceDlqEntry, TraceDlqSummary,
    TraceListRow, Workflow,
};
use crate::errors::OrionError;

pub use orion_api::dto::{
    AuditLogEntryResponse, ChannelResponse, ConnectorResponse, PackageReceiptResponse,
    TraceDlqEntryResponse, TraceDlqSummaryResponse, TraceListItemResponse, WorkflowResponse,
};

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
            // Wire name `tags`, column name `tags_json` (D26); the label is
            // the column, because that is what an operator staring at a
            // corrupt row has to go and look at.
            tags: parse_json_field(&workflow.tags_json, "workflow", id, "tags_json")?,
            continue_on_error: workflow.continue_on_error,
            content_hash: crate::storage::content::content_hash(
                &crate::storage::content::workflow_content(workflow)?,
            ),
            created_at: workflow.created_at,
            updated_at: workflow.updated_at,
        })
    }
}

impl TryFrom<&Channel> for ChannelResponse {
    type Error = OrionError;

    fn try_from(channel: &Channel) -> Result<Self, Self::Error> {
        let id = &channel.channel_id;
        // Wire name `methods`, column name `methods_json` (D26).
        let methods = channel
            .methods_json
            .as_ref()
            .map(|m| parse_json_field(m, "channel", id, "methods_json"))
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
            // H3: `auth.keys` / `auth.secret` may be stored literal, and this
            // is the only constructor of the wire shape — masking here makes
            // it a step no handler can skip, exactly like `mask_connector`.
            config: {
                let mut config =
                    parse_json_field(&channel.config_json, "channel", id, "config_json")?;
                crate::connector::mask_channel_config(&mut config);
                config
            },
            status: channel.status.clone(),
            priority: channel.priority,
            tags: parse_json_field(&channel.tags_json, "channel", id, "tags_json")?,
            content_hash: crate::storage::content::content_hash(
                &crate::storage::content::channel_content(channel)?,
            ),
            created_at: channel.created_at,
            updated_at: channel.updated_at,
        })
    }
}

/// A connector as the admin API shows it: the stored document verbatim, but
/// the only supported way to build one is [`crate::connector::mask_connector`],
/// which replaces every secret with `******` first. The row struct it is built
/// from carries the *unmasked* config, and cannot be serialized (D27), so a
/// handler that forgets to mask no longer compiles.
impl From<&Connector> for ConnectorResponse {
    fn from(connector: &Connector) -> Self {
        Self {
            id: connector.id.clone(),
            name: connector.name.clone(),
            connector_type: connector.connector_type.clone(),
            config_json: connector.config_json.clone(),
            enabled: connector.enabled,
            // Tolerant, unlike the channel/workflow decode: this `From` is
            // infallible because `mask_connector` (the sole constructor of
            // the wire shape) must never fail, and tags are advisory
            // selection labels — a corrupt value must not take down every
            // connector read, list and export.
            tags: serde_json::from_str(&connector.tags_json)
                .unwrap_or_else(|_| Value::Array(Vec::new())),
            content_hash: crate::storage::content::connector_content(connector)
                .map(|v| crate::storage::content::content_hash(&v))
                .unwrap_or_default(),
            created_at: connector.created_at,
            updated_at: connector.updated_at,
        }
    }
}

impl From<&PackageReceipt> for PackageReceiptResponse {
    fn from(receipt: &PackageReceipt) -> Self {
        Self {
            name: receipt.name.clone(),
            version: receipt.version.clone(),
            content_hash: receipt.content_hash.clone(),
            state: receipt.state.clone(),
            principal: receipt.principal.clone(),
            created_at: receipt.created_at,
            updated_at: receipt.updated_at,
        }
    }
}

impl From<&TraceDlqEntry> for TraceDlqEntryResponse {
    fn from(entry: &TraceDlqEntry) -> Self {
        Self {
            id: entry.id.clone(),
            trace_id: entry.trace_id.clone(),
            channel: entry.channel.clone(),
            payload_json: entry.payload_json.clone(),
            metadata_json: entry.metadata_json.clone(),
            error_message: entry.error_message.clone(),
            retry_count: entry.retry_count,
            max_retries: entry.max_retries,
            next_retry_at: entry.next_retry_at,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}

impl From<&TraceDlqSummary> for TraceDlqSummaryResponse {
    fn from(entry: &TraceDlqSummary) -> Self {
        Self {
            id: entry.id.clone(),
            trace_id: entry.trace_id.clone(),
            channel: entry.channel.clone(),
            error_message: entry.error_message.clone(),
            retry_count: entry.retry_count,
            max_retries: entry.max_retries,
            next_retry_at: entry.next_retry_at,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}

impl From<&TraceListRow> for TraceListItemResponse {
    fn from(trace: &TraceListRow) -> Self {
        Self {
            id: trace.id.clone(),
            channel: trace.channel.clone(),
            channel_id: trace.channel_id.clone(),
            mode: trace.mode.clone(),
            status: trace.status.clone(),
            error_message: trace.error_message.clone(),
            duration_ms: trace.duration_ms,
            started_at: trace.started_at,
            completed_at: trace.completed_at,
            created_at: trace.created_at,
            updated_at: trace.updated_at,
        }
    }
}

impl From<&AuditLogEntry> for AuditLogEntryResponse {
    fn from(entry: &AuditLogEntry) -> Self {
        Self {
            id: entry.id.clone(),
            principal: entry.principal.clone(),
            action: entry.action.clone(),
            resource_type: entry.resource_type.clone(),
            resource_id: entry.resource_id.clone(),
            details: entry.details.clone(),
            created_at: entry.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::enums::{CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC};
    use crate::storage::models::{ChannelProtocol, EntityStatus};
    use chrono::{NaiveDate, NaiveDateTime};

    /// Every column decoded as JSON is named `*_json` (D26).
    ///
    /// `parse_json_field` is the single door a stored column goes through to
    /// become a `serde_json::Value`, and its fourth argument is the column the
    /// caller is decoding — the label an operator sees when a row turns out to
    /// hold something that is not JSON. So the set of columns that are JSON is
    /// exactly the set of labels passed here, and the suffix rule can be
    /// checked against them rather than against a hand-kept list that drifts.
    ///
    /// `workflows.tags` and `channels.methods` were the two columns that
    /// failed this until 1.0.0; they are now `tags_json` and `methods_json`.
    /// A source scan, not a type-level rule, for the same reason
    /// `row_structs_are_not_wire_types` is one: nothing in the type system
    /// says "this `String` is really a JSON document".
    #[test]
    fn json_columns_carry_the_json_suffix() {
        const SOURCE: &str = include_str!("dto.rs");
        const CALL: &str = "parse_json_field(";

        let mut labels = Vec::new();
        // Skip the `const CALL` declaration itself, and this test's own body,
        // by only scanning up to the `mod tests` boundary.
        let code = SOURCE
            .split_once("\n#[cfg(test)]\n")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);

        let mut rest = code;
        while let Some(start) = rest.find(CALL) {
            rest = &rest[start + CALL.len()..];
            // Walk to the matching close paren, splitting the top-level args.
            let mut depth = 0usize;
            let mut args = vec![String::new()];
            let mut end = rest.len();
            for (i, ch) in rest.char_indices() {
                match ch {
                    '(' | '[' => depth += 1,
                    ')' | ']' if depth == 0 => {
                        end = i;
                        break;
                    }
                    ')' | ']' => depth -= 1,
                    ',' if depth == 0 => {
                        args.push(String::new());
                        continue;
                    }
                    _ => {}
                }
                args.last_mut().expect("at least one arg").push(ch);
            }
            // A rustfmt-wrapped call ends `column,\n)`, leaving a blank arg.
            args.retain(|a| !a.trim().is_empty());
            assert_eq!(
                args.len(),
                4,
                "parse_json_field takes (value, entity, id, column); found {} args in \
                 `{}`",
                args.len(),
                &rest[..end.min(120)]
            );
            labels.push(args[3].trim().trim_matches('"').to_string());
            rest = &rest[end..];
        }

        assert!(
            labels.len() >= 6,
            "the scan found only {} parse_json_field call(s) — it has stopped \
             matching the source and is no longer checking anything: {labels:?}",
            labels.len()
        );
        for label in &labels {
            assert!(
                label.ends_with("_json"),
                "`{label}` is decoded as a JSON document but its column is not named \
                 `*_json` (D26). The suffix is the only signal a reader gets that the \
                 value has to go through serde_json before it means anything — every \
                 other column in the schema is used as-is."
            );
        }
        // The two D26 moved must be in there, under their new names.
        for expected in ["tags_json", "methods_json"] {
            assert!(
                labels.iter().any(|l| l == expected),
                "expected `{expected}` among the decoded columns: {labels:?}"
            );
        }
    }

    fn sample_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 1, 1)
            .expect("test")
            .and_hms_opt(0, 0, 0)
            .expect("test")
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
            tags_json: r#"["test"]"#.to_string(),
            continue_on_error: false,
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        }
    }

    fn sample_channel() -> Channel {
        Channel {
            tags_json: "[]".to_string(),
            channel_id: "ch-1".to_string(),
            version: 1,
            name: "orders".to_string(),
            description: Some("Order processing channel".to_string()),
            channel_type: CHANNEL_TYPE_SYNC.to_string(),
            protocol: ChannelProtocol::Rest.as_str().to_string(),
            methods_json: Some(r#"["POST"]"#.to_string()),
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
        let response = WorkflowResponse::try_from(&workflow).expect("test");
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
        workflow.tags_json = "not json".to_string();
        let result = WorkflowResponse::try_from(&workflow);
        assert!(result.is_err());
    }

    #[test]
    fn test_workflow_response_try_from_no_description() {
        let mut workflow = sample_workflow();
        workflow.description = None;
        let response = WorkflowResponse::try_from(&workflow).expect("test");
        assert!(response.description.is_none());
    }

    #[test]
    fn test_channel_response_try_from_valid() {
        let channel = sample_channel();
        let response = ChannelResponse::try_from(&channel).expect("test");
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
        channel.methods_json = None;
        channel.route_pattern = None;
        channel.topic = Some("order.placed".to_string());
        channel.consumer_group = Some("orion".to_string());
        let response = ChannelResponse::try_from(&channel).expect("test");
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

    // -- D27/D28: the conversions must reproduce the pre-split wire shape
    // exactly. These pin the field sets that `Connector`, `AuditLogEntry`,
    // `TraceDlqEntry` and `TraceDlqSummary` published when they were row
    // structs that derived `Serialize` themselves.

    fn field_names(value: &serde_json::Value) -> Vec<String> {
        value
            .as_object()
            .expect("DTO serializes to an object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn connector_response_keeps_the_row_wire_shape() {
        let row = Connector {
            tags_json: "[]".to_string(),
            id: "c-1".to_string(),
            name: "pg".to_string(),
            connector_type: "db".to_string(),
            config_json: r#"{"password":"******"}"#.to_string(),
            enabled: true,
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        };
        let value = serde_json::to_value(ConnectorResponse::from(&row)).expect("test");
        assert_eq!(
            field_names(&value),
            [
                "id",
                "name",
                "connector_type",
                "config_json",
                "enabled",
                "tags",
                "content_hash",
                "created_at",
                "updated_at"
            ]
        );
        assert_eq!(value["config_json"], r#"{"password":"******"}"#);
        assert_eq!(value["tags"], serde_json::json!([]));
        assert!(
            value["content_hash"]
                .as_str()
                .is_some_and(|h| h.starts_with("sha256:")),
            "{value}"
        );
        assert_eq!(value["created_at"], "2025-01-01T00:00:00");
    }

    #[test]
    fn audit_log_response_keeps_the_row_wire_shape() {
        let row = AuditLogEntry {
            id: "a-1".to_string(),
            principal: "admin...".to_string(),
            action: "activate".to_string(),
            resource_type: "workflow".to_string(),
            resource_id: "wf-1".to_string(),
            details: None,
            created_at: sample_datetime(),
        };
        let value = serde_json::to_value(AuditLogEntryResponse::from(&row)).expect("test");
        assert_eq!(
            field_names(&value),
            [
                "id",
                "principal",
                "action",
                "resource_type",
                "resource_id",
                "details",
                "created_at"
            ]
        );
        // `details: None` stayed a present `null` before the split — no
        // `skip_serializing_if` was involved.
        assert!(value["details"].is_null());
    }

    #[test]
    fn trace_dlq_responses_keep_the_row_wire_shapes() {
        let entry = TraceDlqEntry {
            id: "d-1".to_string(),
            trace_id: "t-1".to_string(),
            channel: "orders".to_string(),
            payload_json: r#"{"a":1}"#.to_string(),
            metadata_json: "{}".to_string(),
            error_message: "boom".to_string(),
            retry_count: 1,
            max_retries: 3,
            next_retry_at: sample_datetime(),
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        };
        let value = serde_json::to_value(TraceDlqEntryResponse::from(&entry)).expect("test");
        assert_eq!(
            field_names(&value),
            [
                "id",
                "trace_id",
                "channel",
                "payload_json",
                "metadata_json",
                "error_message",
                "retry_count",
                "max_retries",
                "next_retry_at",
                "created_at",
                "updated_at"
            ]
        );

        let summary = TraceDlqSummary {
            id: "d-1".to_string(),
            trace_id: "t-1".to_string(),
            channel: "orders".to_string(),
            error_message: "boom".to_string(),
            retry_count: 1,
            max_retries: 3,
            next_retry_at: sample_datetime(),
            created_at: sample_datetime(),
            updated_at: sample_datetime(),
        };
        let value = serde_json::to_value(TraceDlqSummaryResponse::from(&summary)).expect("test");
        assert_eq!(
            field_names(&value),
            [
                "id",
                "trace_id",
                "channel",
                "error_message",
                "retry_count",
                "max_retries",
                "next_retry_at",
                "created_at",
                "updated_at"
            ]
        );
        // The listing shape must stay payload-free.
        assert!(value.get("payload_json").is_none());
        assert!(value.get("metadata_json").is_none());
    }
}
