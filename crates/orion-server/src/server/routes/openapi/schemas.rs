//! OpenAPI schema-only response envelope and DTO definitions.
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

// ============================================================
// Response envelopes (R17 shape, R22 schemas)
// ============================================================

/// The `{"data": …}` envelope every admin 2xx carries.
///
/// Generic so each resource gets a described response without a hand-written
/// wrapper struct per endpoint. utoipa inlines the type parameter, so
/// `DataEnvelope<CronOccurrenceResponse>` publishes the full shape. Before R22, 44
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

/// `GET /api/v2/admin/connectors/circuit-breakers`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerStates {
    /// Whether `engine.circuit_breaker.enabled` is set. When false, `breakers`
    /// is empty because nothing is tracked.
    enabled: bool,
    /// Always `"node"` — breaker state is per-replica, never cluster-wide.
    scope: String,
    /// Which node's map this is.
    instance_id: String,
    /// `channel:connector` → `closed` | `open` | `half_open`. Node-local.
    breakers: std::collections::HashMap<String, String>,
}

/// `POST /api/v2/admin/connectors/circuit-breakers/{key}`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct CircuitBreakerReset {
    reset: bool,
    key: String,
    /// Whether the breaker existed on the node that served the request.
    /// Breakers are node-local; in cluster mode the reset reaches every node
    /// regardless, so `false` here is not a failure.
    found_on_this_node: bool,
}

/// `POST /api/v2/admin/trace-dlq/purge`.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct DlqPurgeResult {
    /// Rows deleted.
    purged: u64,
    /// Echo of the requested age bound.
    older_than_hours: i64,
}

/// `POST /api/v2/admin/definitions/test` — one run of a workflow definition.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct WorkflowTestResult {
    /// Whether the workflow's condition matched the supplied input.
    matched: bool,
    /// The run's trace: the context it started from (`initial`), then each
    /// step with its `task_id`, `result` (`executed` | `skipped` | `failed`)
    /// and the changes it made.
    trace: Value,
    /// The message data after the pipeline ran.
    output: Value,
    errors: Vec<Value>,
    /// Why the run stopped short, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The backup `POST /api/v2/admin/backups` just wrote.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupFile {
    filename: String,
    /// Absolute path on the node that served the request.
    path: String,
    size_bytes: u64,
    created_at: String,
}

/// One row of `GET /api/v2/admin/backups`. Deliberately not [`BackupFile`]:
/// the listing reads `mtime` off the directory entry and has no `path`, so
/// the two responses differ by two fields.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct BackupListItem {
    filename: String,
    size_bytes: u64,
    modified_at: String,
}

/// `GET /api/v2/admin/traces` — a page of trace rows.
///
/// Not [`PaginatedEnvelope`]: `total` is present only when the request
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

/// `GET /api/v2/admin/traces/{id}` — the list row plus the payloads.
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
    /// `context.metadata` is stripped from the projection.
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

/// One entry of the function catalogue from `GET /api/v2/admin/functions`.
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
    /// the trace DLQ, a cron occurrence's retry, `http_call`'s transport retry — and
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
    /// and `packages` when `[packages] apply` names artifacts.
    components: Value,
    /// Build provenance and detail, served only to an admin caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_hash: Option<String>,
}
