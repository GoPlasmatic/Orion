//! OpenAPI schema-only response envelope and DTO definitions (`MOD-04`).
//!
//! These endpoints build their responses with `json!`, so there is no runtime
//! struct to point `body =` at. Declaring the shape here is what makes the
//! published document describe them at all; the
//! `every_response_that_can_carry_a_body_describes_one` test keeps it that way.

use serde_json::Value;

use crate::storage::models::TraceListItemResponse;

/// Error response body matching Orion's
/// `{"error": {"code": "...", "message": "...", "request_id": "..."}}` format.
///
/// Every 4xx/5xx from `OrionError` — and the 500 emitted by the panic-recovery
/// layer — uses this envelope.
///
/// These are the shared contract types, not mirrors of them. Three hand-written
/// structs used to stand in here and had already drifted: they published
/// `details` as a nullable array, while `ErrorBody` omits the key when empty and
/// never sends null. `schema(as = …)` on the orion-api types keeps the component
/// names the document has always used, since renaming one breaks clients
/// generated against it.
pub(crate) use orion_api::{
    ErrorBody as ErrorDetail, ErrorEnvelope as ErrorResponse, FieldError as ErrorFieldDetail,
};

// Result of a bulk import (R18, K2). This used to be a schema-only mirror of
// what `import_response` built with `json!`; the shared `orion-api` type is
// now both the runtime shape and the published schema, so the mirror is gone
// and the two cannot drift. Re-exported so `#[utoipa::path]` bodies keep
// referencing it by its old path.
pub(crate) use orion_api::ImportResult;

// ============================================================
// Response envelopes (R17 shape, R22 schemas)
// ============================================================

/// The `{"data": …}` envelope every admin 2xx carries (R17).
///
/// Generic so each resource gets a described response without a hand-written
/// wrapper struct per endpoint. utoipa inlines the type parameter, so
/// `DataEnvelope<WorkflowResponse>` publishes the full shape. Before R22, 44
/// of the 48 2xx responses had no `content` block at all.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct DataEnvelope<T> {
    #[allow(dead_code)] // Schema-only: the runtime builds this with `json!`.
    pub(crate) data: T,
}

/// The paginated envelope: `data` plus the three counters, and nothing else.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PaginatedEnvelope<T> {
    #[allow(dead_code)]
    pub(crate) data: Vec<T>,
    /// Total rows matching the filter, ignoring `limit`/`offset`.
    #[allow(dead_code)]
    pub(crate) total: i64,
    #[allow(dead_code)]
    pub(crate) limit: i64,
    #[allow(dead_code)]
    pub(crate) offset: i64,
}

/// One row of `GET /api/v1/admin/connectors`: the stored connector with
/// secrets masked, plus the registry's view of whether it actually loaded.
///
/// Declared separately from [`crate::storage::models::ConnectorResponse`]
/// because `PaginatedEnvelope<ConnectorResponse>` would under-describe the row
/// by exactly the three fields an operator reads the list for.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct ConnectorListItem {
    id: String,
    name: String,
    connector_type: String,
    /// Connector config with every secret replaced by `******`, verbatim.
    config_json: String,
    /// The same masked config, parsed — the shape `POST`/`PUT` accept.
    config: serde_json::Value,
    enabled: bool,
    /// Selection labels (K6); filter the list with `?tag=`.
    tags: Vec<String>,
    /// `sha256:…` over the canonical importable content (K10).
    content_hash: String,
    created_at: String,
    updated_at: String,
    /// `loaded`, `failed`, or `disabled`.
    #[schema(example = "loaded")]
    load_status: String,
    /// Why the connector failed to load. Present only when
    /// `load_status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    load_error: Option<String>,
    /// Which load stage failed. Present only when `load_status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    load_error_stage: Option<String>,
}

/// One item of `GET /api/v1/admin/connectors/export`.
///
/// Declared separately from [`crate::storage::models::ConnectorResponse`]
/// because an export has to emit the shape `/import` *accepts*, not the shape
/// the read endpoints return: `config` is the parsed object here, where the
/// read shape carries `config_json` as a string. Publishing the read type for
/// this route promised three fields the body does not carry (`config_json`,
/// `created_at`, `updated_at`) and hid the one it does.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct ConnectorExportItem {
    id: String,
    name: String,
    connector_type: String,
    /// The parsed connector config, with every secret replaced by `******`.
    /// `env://`-style references survive the masking as references.
    config: serde_json::Value,
    enabled: bool,
    /// Selection labels (K6); filter the export with `?tag=`.
    tags: serde_json::Value,
    /// `sha256:…` over the canonical importable content (K10), computed on the
    /// stored (unmasked) config. Ignored by `/import`, like the other extras.
    content_hash: String,
}

/// `GET /api/v1/admin/connectors/circuit-breakers`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerStates {
    /// Whether `engine.circuit_breaker.enabled` is set. When false, `breakers`
    /// is empty because nothing is tracked.
    enabled: bool,
    /// Always `"node"` — breaker state is per-replica, never cluster-wide (F21).
    scope: String,
    /// Which node's map this is.
    instance_id: String,
    /// `channel:connector` → `closed` | `open` | `half_open`. Node-local.
    breakers: std::collections::HashMap<String, String>,
}

/// `POST /api/v1/admin/connectors/circuit-breakers/{key}`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerReset {
    reset: bool,
    key: String,
    /// Whether the breaker existed on the node that served the request.
    /// Breakers are node-local; in cluster mode the reset is broadcast over
    /// the epoch bus regardless, so `false` here is not a failure (F21).
    found_on_this_node: bool,
}

/// `POST /api/v1/admin/trace-dlq/purge`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct DlqPurgeResult {
    /// Rows deleted.
    purged: u64,
    /// Echo of the requested age bound.
    older_than_hours: i64,
}

/// `POST /api/v1/admin/workflows/{id}/test` — a dry run against sample input.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct WorkflowTestResult {
    /// Whether the workflow's condition matched the supplied input.
    matched: bool,
    /// Per-step execution trace: `task_id` and `result`
    /// (`executed` | `skipped` | `failed`) for each step.
    trace: Value,
    /// The message data after the pipeline ran.
    output: Value,
    errors: Vec<Value>,
}

/// The backup `POST /api/v1/admin/backups` just wrote.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupFile {
    filename: String,
    /// Absolute path on the node that served the request.
    path: String,
    size_bytes: u64,
    created_at: String,
}

/// One row of `GET /api/v1/admin/backups`. Deliberately not [`BackupFile`]:
/// the listing reads `mtime` off the directory entry and has no `path`, so
/// the two responses differ by two fields.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupListItem {
    filename: String,
    size_bytes: u64,
    modified_at: String,
}

/// `GET /api/v1/admin/traces` — a page of trace rows.
///
/// Not [`PaginatedEnvelope`] (D8): `total` is present only when the request
/// asked for it with `include_total=true`, because the count scans the whole
/// filtered set; and `next_cursor` carries the keyset position of the last row
/// so the next page can be fetched without an `offset` skip.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct TracePageEnvelope {
    data: Vec<TraceListItemResponse>,
    /// Rows matching the filter, ignoring `limit`/`offset`. Present only with
    /// `include_total=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<i64>,
    limit: i64,
    offset: i64,
    /// Pass back as `?cursor=` to fetch the next page. Present when the page
    /// is ordered by `created_at` (the default) and may have a successor.
    /// Opaque — do not parse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// `GET /api/v1/admin/traces/{id}` — the list row plus the payloads.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct TraceDetail {
    id: String,
    status: String,
    mode: String,
    channel: String,
    channel_id: Option<String>,
    created_at: String,
    /// The full engine message. Present only when `status = "completed"`.
    /// `context.metadata` is stripped from the projection (S14).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Value>,
    /// Present only when `status = "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    /// Per-task execution trace, when the channel enabled `task_details`.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_trace_json: Option<Value>,
}

/// One entry of the function catalogue from `GET /api/v1/admin/functions`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct FunctionSchemaItem {
    name: String,
    description: String,
    /// `connector`, `control`, `data`, `compute`, or `utility`.
    category: String,
    /// `orion` for a handler Orion implements and input-schema validates,
    /// `engine` for a dataflow-rs built-in the engine executes itself.
    source: String,
    /// Other accepted spellings — `validation` carries `validate`. Omitted
    /// when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    /// **Absent** for an engine built-in, which declares no input schema and
    /// is therefore not input-validated at create time.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_fields: Option<Vec<Value>>,
    /// What a second run of this function does, as
    /// `{"kind": "..."}` — `pure`, `read`, `idempotent_write`, `unsafe_write`,
    /// or `depends_on` with the `input` that decides it (`http_call` on its
    /// `method`, `data_write` on its `op`).
    ///
    /// A different question from whether an *error* was transient: this is
    /// what a retry costs when it is. Orion retries tasks in several places —
    /// the trace DLQ, a Kafka redelivery, `http_call`'s transport retry — and
    /// this is how an author or a tool knows which of those are safe over a
    /// given function.
    retry_safety: Value,
}

/// A liveness / readiness / health probe body.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct HealthStatus {
    /// `ok` | `degraded`.
    status: String,
    version: String,
    uptime_seconds: i64,
    /// Per-subsystem state: `database`, `engine`, `connectors`, `channels`,
    /// plus `kafka` when `kafka.enabled` (O10), `cluster_redis` in
    /// cluster mode, and `packages` when `[packages] apply` names artifacts.
    components: Value,
    /// Build provenance and detail, served only to an admin caller (O9).
    #[serde(skip_serializing_if = "Option::is_none")]
    git_hash: Option<String>,
}
