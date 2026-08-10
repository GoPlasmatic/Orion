//! Response DTOs: exactly what the HTTP API serves for a stored entity.
//!
//! Every type here is what an endpoint returns, so it is what the OpenAPI
//! document describes (R22) and what a client deserializes. The server builds
//! each one from its database row through a `From`/`TryFrom` conversion that
//! lives server-side (in `orion-server`'s `storage::models::dto`), next to
//! the row types — that conversion is the only door between the database and
//! a response body.
//!
//! Two shapes exist for the same table where the list view is deliberately
//! narrower than the single-item view — see [`TraceDlqSummaryResponse`].

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// API-friendly representation of a Workflow with parsed JSON fields.
///
/// This — not the server's `Workflow` row struct — is what every workflow
/// endpoint returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
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
    /// `sha256:…` over the canonical importable content (K10) — the same
    /// projection the upsert import compares and the package CLI hashes, so
    /// equal hashes mean "importing one over the other is a no-op".
    pub content_hash: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// API-friendly representation of a Channel with parsed JSON fields.
///
/// This — not the server's `Channel` row struct — is what every channel
/// endpoint returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
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
    /// Channel config with `auth.keys` / `auth.secret` values masked (H3) —
    /// the server's conversion is the only constructor of this shape, so
    /// masking is a step no handler can skip.
    pub config: Value,
    pub status: String,
    pub priority: i64,
    /// Wire name `tags`, stored column `tags_json` (K6) — the same contract
    /// workflows have carried since 0.x.
    pub tags: Value,
    /// `sha256:…` over the canonical importable content (K10). Computed on
    /// the stored (unmasked) config: entities authored with `env://`
    /// references hash identically to their exported artifact; entities
    /// holding literal secrets hash over what they store, which a masked
    /// export can never reproduce.
    pub content_hash: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// A connector as the admin API shows it.
///
/// `config_json` stays a string — it is the stored document verbatim — but
/// with every secret replaced by `******`: the server's `mask_connector` is
/// the only supported way to build one, and the unmasked row struct cannot be
/// serialized (D27), so a handler that forgets to mask does not compile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ConnectorResponse {
    pub id: String,
    pub name: String,
    pub connector_type: String,
    /// Connector config with every secret replaced by `******`.
    pub config_json: String,
    pub enabled: bool,
    /// Wire name `tags`, stored column `tags_json` (K6).
    pub tags: Value,
    /// `sha256:…` over the canonical importable content (K10), computed on
    /// the stored (unmasked) config — see the note on
    /// [`ChannelResponse::content_hash`]. Empty for a row whose stored JSON
    /// no longer parses: corrupt content equals nothing.
    pub content_hash: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// A package receipt as the admin API shows it (K14).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PackageReceiptResponse {
    pub name: String,
    pub version: String,
    /// Canonical content hash of the artifact this receipt records, e.g.
    /// `sha256:…`. Opaque to the server — compared for equality, never parsed.
    pub content_hash: String,
    /// `staged` or `applied`.
    #[cfg_attr(feature = "utoipa", schema(example = "applied"))]
    pub state: String,
    /// Who recorded this receipt (admin key id, or `anonymous`).
    pub principal: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// One DLQ entry with its failed payload — `GET`/`requeue` on a single id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TraceDlqEntryResponse {
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

/// One row of the DLQ listing: [`TraceDlqEntryResponse`] minus the payloads.
///
/// The two differ on purpose — a listing that carried `payload_json` would
/// return every failed request's body in one response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TraceDlqSummaryResponse {
    pub id: String,
    pub trace_id: String,
    pub channel: String,
    pub error_message: String,
    pub retry_count: i64,
    pub max_retries: i64,
    pub next_retry_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// One row of `GET /api/v1/admin/traces` — payload-free by design (S14).
///
/// The narrower sibling of the single-trace read, for the same reason
/// [`TraceDlqSummaryResponse`] is narrower than [`TraceDlqEntryResponse`]: a
/// listing that carried `input_json`/`result_json` would return every
/// caller's request body and the full engine message in one response. The
/// server's `SELECT` already omits those columns, so this type is the second
/// half of that guarantee rather than a filter applied after the fact.
///
/// Published as `TraceListItem`, the name the spec has always used for this
/// component: the Rust type gained the `…Response` suffix every dto here
/// carries, but a component rename would break clients generated against the
/// old spec — the upgrade guide tracks those as a compat surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = TraceListItem))]
pub struct TraceListItemResponse {
    pub id: String,
    /// Channel name as it was when the trace ran — a snapshot, not a key.
    pub channel: String,
    pub channel_id: Option<String>,
    /// `sync` | `async`.
    pub mode: String,
    /// `pending` | `running` | `completed` | `failed`.
    pub status: String,
    pub error_message: Option<String>,
    pub duration_ms: Option<f64>,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// One row of `GET /api/v1/admin/audit-logs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AuditLogEntryResponse {
    pub id: String,
    pub principal: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub details: Option<String>,
    pub created_at: NaiveDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The datetime wire format is chrono's NaiveDateTime serde default —
    /// `2025-01-01T00:00:00` — which is what the server has always sent.
    #[test]
    fn datetimes_parse_the_server_wire_format() {
        let entry: AuditLogEntryResponse = serde_json::from_str(
            r#"{
                "id": "a-1", "principal": "admin", "action": "activate",
                "resource_type": "workflow", "resource_id": "wf-1",
                "details": null, "created_at": "2025-01-01T00:00:00"
            }"#,
        )
        .expect("test");
        assert_eq!(entry.created_at.to_string(), "2025-01-01 00:00:00");
    }

    /// A client parses what the server serves — including fields this crate
    /// version doesn't know yet (unknown fields are ignored by default).
    #[test]
    fn workflow_response_roundtrips_and_tolerates_new_fields() {
        let json = r#"{
            "workflow_id": "wf-1", "version": 1, "name": "n", "description": null,
            "priority": 0, "status": "active", "rollout_percentage": 100,
            "condition": {"==": [1, 1]}, "tasks": [], "tags": [],
            "continue_on_error": false, "content_hash": "sha256:abc",
            "created_at": "2025-01-01T00:00:00", "updated_at": "2025-01-01T00:00:00",
            "some_future_field": true
        }"#;
        let wf: WorkflowResponse = serde_json::from_str(json).expect("test");
        assert_eq!(wf.workflow_id, "wf-1");
        assert_eq!(wf.status, crate::enums::STATUS_ACTIVE);
        let back = serde_json::to_value(&wf).expect("test");
        assert_eq!(back["condition"], serde_json::json!({"==": [1, 1]}));
    }
}
