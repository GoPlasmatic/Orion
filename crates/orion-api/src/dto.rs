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

//! Every field carries `#[serde(default)]` paired with a `schema(required)`
//! override, for the reason spelled out in [`crate::import`]: the client half
//! of the contract is that a response from a server one release away still
//! parses, and the server half is that the published document keeps promising
//! the field, because the server always sends it. Without the pair, a field
//! added in a 1.x minor would be a hard parse error in every older client —
//! which is exactly the skew this crate exists to absorb.
//!
//! Timestamps are `NaiveDateTime` and serialize in chrono's default form
//! (`2026-01-01T00:00:00`) — UTC with no zone designator. That is deliberate
//! and pinned by `row_dto_wire_shape_test`, so the fields are published as a
//! plain `string`: claiming `format: date-time` would promise a zone the wire
//! does not carry, and generated clients would fail to parse it.

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
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub workflow_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub priority: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub rollout_percentage: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub condition: Value,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub tasks: Value,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub tags: Value,
    /// The engine-managed loop over `tasks`, or absent for a workflow that
    /// runs its task list exactly once. Skipped on serialize rather than sent
    /// as `null`, so a response from a server that has the feature and one
    /// from a server that does not are byte-identical for the common case.
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_config: Option<Value>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub continue_on_error: bool,
    /// `sha256:…` over the canonical importable content (K10) — the same
    /// projection the upsert import compares and the package CLI hashes, so
    /// equal hashes mean "importing one over the other is a no-op".
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub content_hash: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// One version of a plugin, as every plugin endpoint returns it.
///
/// `manifest` is the validated manifest as JSON — what was uploaded, with
/// nothing the server inferred added to it. `functions` is the list of
/// names it declares, repeated at the top level so a client need not walk
/// the manifest to learn what the plugin adds to the vocabulary. `health` is
/// present only on the single-entity read, and only when the serving node
/// has an opinion: it says whether *this node* loaded the digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PluginResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub plugin_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    /// `sha256:…` of the component bytes — the identity a generation, a
    /// trace, a package and the catalogue all name the artifact by.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub digest: String,
    /// The WIT package version the component was built against.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub abi: String,
    /// The author's own version string from the manifest, informational.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub plugin_version: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub manifest: Value,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub functions: Vec<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub tags: Value,
    /// `sha256:…` over the importable content (manifest, digest, tags) — the
    /// same projection the upsert import compares.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub content_hash: String,
    /// This node's load state for the version, on the single-entity read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<PluginHealth>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// Whether the node answering loaded a plugin version, and if not, why.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PluginHealth {
    /// `loaded`, `failed`, `disabled` (plugins are off on this node) or
    /// `inactive` (the version is not the one this node's generation carries).
    #[serde(default)]
    pub state: String,
    /// Compile time in milliseconds when loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_ms: Option<u64>,
    /// The stage and reason when `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// API-friendly representation of a Channel with parsed JSON fields.
///
/// This — not the server's `Channel` row struct — is what every channel
/// endpoint returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ChannelResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_type: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub protocol: String,
    #[serde(default)]
    pub methods: Option<Value>,
    #[serde(default)]
    pub route_pattern: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub consumer_group: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub transport_config: Value,
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// Channel config with `auth.keys` / `auth.secret` values masked (H3) —
    /// the server's conversion is the only constructor of this shape, so
    /// masking is a step no handler can skip.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub config: Value,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub priority: i64,
    /// Wire name `tags`, stored column `tags_json` (K6) — the same contract
    /// workflows have carried since 0.x.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub tags: Value,
    /// `sha256:…` over the canonical importable content (K10). Computed on
    /// the stored (unmasked) config: entities authored with `env://`
    /// references hash identically to their exported artifact; entities
    /// holding literal secrets hash over what they store, which a masked
    /// export can never reproduce.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub content_hash: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
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
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub name: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub connector_type: String,
    /// Connector config with every secret replaced by `******`, as the stored
    /// document verbatim.
    ///
    /// Kept for the life of the 1.x line, but `config` is the field to read:
    /// both `POST` and `PUT` take the config as an *object*, so a client that
    /// reads this one has to `JSON.parse` it before it can write it back.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub config_json: String,
    /// The same masked config, parsed — the shape `POST`/`PUT` accept, so a
    /// read response can be edited and written straight back.
    ///
    /// `null` only when the stored document does not parse, which is the same
    /// condition that empties `content_hash`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub config: Value,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub enabled: bool,
    /// Wire name `tags`, stored column `tags_json` (K6).
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub tags: Value,
    /// `sha256:…` over the canonical importable content (K10), computed on
    /// the stored (unmasked) config — see the note on
    /// [`ChannelResponse::content_hash`]. Empty for a row whose stored JSON
    /// no longer parses: corrupt content equals nothing.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub content_hash: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// A package receipt as the admin API shows it (K14).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PackageReceiptResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub name: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: String,
    /// Canonical content hash of the artifact this receipt records, e.g.
    /// `sha256:…`. Opaque to the server — compared for equality, never parsed.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub content_hash: String,
    /// `staged` or `applied`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, example = "applied"))]
    pub state: String,
    /// Who recorded this receipt (admin key id, or `anonymous`).
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub principal: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// One DLQ entry with its failed payload — `GET`/`requeue` on a single id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TraceDlqEntryResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub trace_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub payload_json: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub metadata_json: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub error_message: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub retry_count: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub max_retries: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub next_retry_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// One row of the DLQ listing: [`TraceDlqEntryResponse`] minus the payloads.
///
/// The two differ on purpose — a listing that carried `payload_json` would
/// return every failed request's body in one response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TraceDlqSummaryResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub trace_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub error_message: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub retry_count: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub max_retries: i64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub next_retry_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
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
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    /// Channel name as it was when the trace ran — a snapshot, not a key.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    /// `sync` | `async`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub mode: String,
    /// `pending` | `running` | `completed` | `failed`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(value_type = String))]
    pub started_at: Option<NaiveDateTime>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(value_type = String))]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// One row of `GET /api/v1/admin/audit-logs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct AuditLogEntryResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub principal: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub action: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub resource_type: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub resource_id: String,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
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

    /// Every response DTO parses from `{}`.
    ///
    /// This is the crate-level promise made concrete: a client one release
    /// behind a server that has *dropped* or renamed a field must degrade to a
    /// default, not fail the whole response. Paired with the unknown-field
    /// tolerance below, it makes skew in both directions a non-event.
    #[test]
    fn every_response_dto_parses_from_an_empty_object() {
        macro_rules! parses_empty {
            ($($t:ty),+ $(,)?) => {$(
                serde_json::from_str::<$t>("{}").unwrap_or_else(|e| {
                    panic!("{} must tolerate a missing field: {e}", stringify!($t))
                });
            )+};
        }
        parses_empty!(
            WorkflowResponse,
            ChannelResponse,
            ConnectorResponse,
            PackageReceiptResponse,
            TraceDlqEntryResponse,
            TraceDlqSummaryResponse,
            TraceListItemResponse,
            AuditLogEntryResponse,
        );
    }

    /// A vocabulary that grows in a 1.x minor does not break an older client.
    ///
    /// The response fields that hold a growing set are `String`, not the
    /// [`crate::enums`] types, precisely so this parses. If someone ever
    /// "tightens" one of these into an enum, this test is what says no.
    #[test]
    fn an_unknown_status_value_still_parses() {
        let wf: WorkflowResponse =
            serde_json::from_str(r#"{"workflow_id": "wf-1", "status": "quarantined"}"#)
                .expect("an unrecognised status must not fail the whole response");
        assert_eq!(wf.status, "quarantined");

        let receipt: PackageReceiptResponse =
            serde_json::from_str(r#"{"name": "p", "state": "rolling_back"}"#)
                .expect("an unrecognised package state must not fail the response");
        assert_eq!(receipt.state, "rolling_back");
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
