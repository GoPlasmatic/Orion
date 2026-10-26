//! Response DTOs: exactly what the HTTP API serves for a stored entity.
//!
//! Every type here is what an endpoint returns, so it is what the OpenAPI
//! document describes and what a client deserializes. The server builds
//! each one from its database row through a `From`/`TryFrom` conversion that
//! lives server-side (in `orion-server`'s `storage::models::dto`), next to
//! the row types — that conversion is the only door between the database and
//! a response body.
//!
//! Two shapes exist for the same table where the list view is deliberately
//! narrower than the single-item view — see [`TraceDlqSummaryResponse`].

//! Every field carries `#[serde(default)]` paired with a `schema(required)`
//! override: the client half
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

/// One row of `GET /api/v2/admin/cron/occurrences`.
///
/// The list projection: what happened and when, without the diagnostic detail.
/// Narrower than [`CronOccurrenceResponse`] for readability rather than for
/// cost — an occurrence carries no payload column, unlike a trace — so a page
/// of a hundred reads as a schedule's history instead of as a wall of lease
/// bookkeeping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CronOccurrenceSummaryResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_id: String,
    /// The channel's name when this occurrence was materialised, not its name
    /// now — renaming a channel does not rewrite what already ran.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_name: String,
    /// `cron` or `manual`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub trigger: String,
    /// The immutable UTC instant this occurrence was due.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub scheduled_for: NaiveDateTime,
    /// `pending`, `claimed`, `running`, `completed`, `failed`,
    /// `skipped_misfire` or `skipped_singleton`. An open string on the wire:
    /// tolerate a value you do not know.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub attempt: i64,
    #[serde(default)]
    pub started_at: Option<NaiveDateTime>,
    #[serde(default)]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
}

/// One occurrence in full: `GET /api/v2/admin/cron/occurrences/{id}`.
///
/// Adds what the summary leaves out — the failure reason, the trace to read the
/// run in, and the lease bookkeeping that answers "which node has it, and until
/// when?".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CronOccurrenceResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_name: String,
    /// The channel version that materialised this occurrence…
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_version: i64,
    /// …and the one that claimed it. They differ when the channel gained a
    /// version between the two, which is expected: a pending occurrence follows
    /// the active generation at claim time, exactly as a queued async trace
    /// does. `null` until claimed.
    #[serde(default)]
    pub executing_version: Option<i64>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub trigger: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub scheduled_for: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub attempt: i64,
    /// The instance holding this occurrence, and until when. Both `null` unless
    /// an attempt is in flight.
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default)]
    pub claimed_until: Option<NaiveDateTime>,
    #[serde(default)]
    pub singleton_key: Option<String>,
    /// The acquisition generation this attempt holds its key under. Diagnostic:
    /// Orion's connectors do not accept a fencing token, so it bounds Orion's
    /// own writes rather than a downstream side effect.
    #[serde(default)]
    pub fencing_token: Option<i64>,
    /// Which of the key's `concurrency.slots` this attempt holds, from `0`.
    /// Two occurrences of one key can share a fencing token when they hold
    /// different slots, so the pair is what names a hold. `null` under
    /// `allow`.
    #[serde(default)]
    pub singleton_slot: Option<i64>,
    /// The trace this attempt wrote, readable at
    /// `GET /api/v2/admin/traces/{id}`. `null` before admission, and when trace
    /// storage dropped the row — the occurrence is kept either way, because it
    /// is scheduling correctness state rather than optional observability.
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub started_at: Option<NaiveDateTime>,
    #[serde(default)]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub updated_at: NaiveDateTime,
}

/// One channel's schedule as the runtime sees it right now:
/// `GET /api/v2/admin/cron/status`.
///
/// Deliberately a separate response rather than fields on the channel. The
/// channel row is an immutable authored definition; where its cursor has got to
/// is runtime state that changes every minute, and writing it into the active
/// row would make an immutable record mutable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CronScheduleStatusResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel_name: String,
    /// The authored expression and zone, echoed so a status read answers
    /// "what is scheduled?" without a second request.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub schedule: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub timezone: String,
    /// The next UTC instant this schedule will materialise. `null` when the
    /// reconciler has not yet seen the channel — it appears within one poll
    /// interval of activation.
    #[serde(default)]
    pub next_fire_at: Option<NaiveDateTime>,
    /// Set when the channel has left the active set. Its history is kept; it
    /// simply stops producing occurrences.
    #[serde(default)]
    pub paused_at: Option<NaiveDateTime>,
    /// The most recent occurrence's status and instant, so one read answers
    /// "is this schedule healthy?".
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_scheduled_for: Option<NaiveDateTime>,
    #[serde(default)]
    pub last_completed_at: Option<NaiveDateTime>,
    /// Occurrences waiting for a worker. A number that only grows is the signal
    /// that this schedule is producing work faster than the instance runs it.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub pending: i64,
    /// `allow` or `forbid`.
    #[serde(default)]
    pub concurrency_policy: String,
    /// The lock this channel's runs take. Present under `forbid`.
    #[serde(default)]
    pub singleton_key: Option<String>,
    /// How many runs of `singleton_key` this channel admits at once. Present
    /// under `forbid`.
    #[serde(default)]
    pub slots: Option<u32>,
    /// Live leases on `singleton_key` right now, across every channel that
    /// shares the key — so it can exceed this channel's `slots` when another
    /// channel declares more, or just after `slots` was lowered.
    #[serde(default)]
    pub slots_held: Option<u32>,
}

/// One row of `GET /api/v2/admin/traces` — payload-free by design.
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

/// One row of `GET /api/v2/admin/audit-logs`.
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

// ============================================================
// Operational and admin-action responses
// ============================================================

/// This node's circuit breakers (`GET /api/v2/admin/connectors/circuit-breakers`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CircuitBreakerStatesResponse {
    /// Whether `engine.circuit_breaker.enabled` is set. When false, `breakers`
    /// is empty because nothing is tracked.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub enabled: bool,
    /// Always `"node"` — breaker state is per-replica, never cluster-wide.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub scope: String,
    /// Which node's map this is.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub instance_id: String,
    /// `channel:connector` → `closed` | `open` | `half_open`. Node-local.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub breakers: std::collections::BTreeMap<String, String>,
}

/// A reset breaker (`POST /api/v2/admin/connectors/circuit-breakers/{key}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CircuitBreakerResetResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub reset: bool,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub key: String,
    /// Whether the breaker existed on the node that served the request.
    /// Breakers are node-local; in cluster mode the reset reaches every node
    /// regardless, so `false` here is not a failure.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub found_on_this_node: bool,
}

/// What a DLQ purge deleted (`POST /api/v2/admin/trace-dlq/purge`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DlqPurgeResponse {
    /// Rows deleted.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub purged: u64,
    /// Echo of the requested age bound.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub older_than_hours: u64,
}

/// The backup `POST /api/v2/admin/backups` just wrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct BackupFileResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub filename: String,
    /// Absolute path on the node that served the request.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub path: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub size_bytes: u64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub created_at: String,
}

/// One row of `GET /api/v2/admin/backups`. Deliberately not
/// [`BackupFileResponse`]: the listing reads `mtime` off the directory entry
/// and has no `path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct BackupListItemResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub filename: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub size_bytes: u64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub modified_at: String,
}

/// `GET /api/v2/admin/traces/{id}` — the list row plus the payloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct TraceDetailResponse {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub id: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub mode: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub channel: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required, value_type = String))]
    pub created_at: NaiveDateTime,
    /// The full engine message. Present only when `status = "completed"`.
    /// `context.metadata` is stripped from the projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<serde_json::Value>,
    /// Present only when `status = "failed"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "utoipa", schema(value_type = Option<String>))]
    pub started_at: Option<NaiveDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "utoipa", schema(value_type = Option<String>))]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    /// Per-task execution trace, when the channel enabled `task_details`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_trace_json: Option<serde_json::Value>,
}

/// `GET /healthz`: the process is serving.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct LivenessResponse {
    /// Always `ok`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
}

/// `GET /readyz`: whether this node should take traffic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ReadinessResponse {
    /// `ready` | `not_ready`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub components: ReadinessComponents,
}

/// The parts of a [`ReadinessResponse`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ReadinessComponents {
    /// `ok` | `error`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub database: String,
    /// Always `ok`: the engine snapshot is lock-free and cannot be
    /// unavailable once the process serves. Kept for shape stability.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub engine: String,
    /// Whether startup has completed, `[packages] apply` included.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub initialized: bool,
    /// `ok` | `degraded` | `error`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub background_tasks: String,
}

/// `GET /health`: the detailed report. The fields after `components` are
/// served only to a caller that may read the admin plane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct HealthResponse {
    /// `ok` | `degraded`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub status: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub version: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub uptime_seconds: i64,
    /// Per-subsystem state, each `ok`, `degraded`, `error` or `disabled`:
    /// `database`, `engine`, `connectors`, `channels`, `background_tasks`,
    /// `engine_reload`, `plugins`, and `cron` when this node schedules.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub components: std::collections::BTreeMap<String, String>,
    /// Build provenance and detail, served only to an admin caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows_loaded: Option<usize>,
    /// The breaker map (node-local) and the connectors that failed to load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<serde_json::Value>,
    /// The quarantined channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<serde_json::Value>,
    /// The plugins loaded, and those that failed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<serde_json::Value>,
    /// The scheduler's own health, on a node that schedules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<serde_json::Value>,
    /// Each background task's state and restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tasks: Option<serde_json::Value>,
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
            TraceDlqEntryResponse,
            TraceDlqSummaryResponse,
            TraceListItemResponse,
            AuditLogEntryResponse,
        );
    }
}
