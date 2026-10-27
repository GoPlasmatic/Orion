//! Row → DTO conversions: how a stored row becomes what the HTTP API serves.
//!
//! The DTO shapes themselves live in the shared `orion-api` crate (they are
//! the wire contract, and the CLI deserializes them) and are re-exported here
//! under their established paths. What this module holds is every
//! `From`/`TryFrom` from a row in [`super::rows`] into a DTO. That conversion
//! is the only door between the database and a response body, which is what
//! makes "which columns are public?" answerable by reading one file (D27, D28).

use super::rows::{AuditLogEntry, CronOccurrence, TraceDlqEntry, TraceDlqSummary, TraceListRow};

pub use orion_api::dto::{
    AuditLogEntryResponse, CronOccurrenceResponse, CronOccurrenceSummaryResponse,
    CronScheduleStatusResponse, TraceDlqEntryResponse, TraceDlqSummaryResponse,
    TraceListItemResponse,
};

impl From<&CronOccurrence> for CronOccurrenceSummaryResponse {
    fn from(row: &CronOccurrence) -> Self {
        Self {
            id: row.id.clone(),
            channel_id: row.channel_id.clone(),
            channel_name: row.channel_name.clone(),
            trigger: row.trigger.clone(),
            scheduled_for: row.scheduled_for,
            status: row.status.clone(),
            attempt: row.attempt,
            started_at: row.started_at,
            completed_at: row.completed_at,
            created_at: row.created_at,
        }
    }
}

impl From<&CronOccurrence> for CronOccurrenceResponse {
    fn from(row: &CronOccurrence) -> Self {
        Self {
            id: row.id.clone(),
            channel_id: row.channel_id.clone(),
            channel_name: row.channel_name.clone(),
            channel_version: row.channel_version,
            executing_version: row.executing_version,
            workflow_id: row.workflow_id.clone(),
            trigger: row.trigger.clone(),
            scheduled_for: row.scheduled_for,
            status: row.status.clone(),
            attempt: row.attempt,
            claimed_by: row.claimed_by.clone(),
            claimed_until: row.claimed_until,
            singleton_key: row.singleton_key.clone(),
            fencing_token: row.fencing_token,
            singleton_slot: row.singleton_slot,
            trace_id: row.trace_id.clone(),
            error_message: row.error_message.clone(),
            started_at: row.started_at,
            completed_at: row.completed_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
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
    use chrono::{NaiveDate, NaiveDateTime};

    fn sample_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 1, 1)
            .expect("test")
            .and_hms_opt(0, 0, 0)
            .expect("test")
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
