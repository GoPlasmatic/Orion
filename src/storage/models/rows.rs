//! Row structs: exactly what a `SELECT` decodes into, and nothing else.
//!
//! # The rule
//!
//! **A row struct never derives `Serialize` or `ToSchema`** (D27, D28). It is a
//! picture of a table, so it holds whatever the table holds — including columns
//! that exist to be compared, never shown, like [`Trace::access_token_hash`].
//! The moment such a struct is also a wire type, "does this field leave the
//! process?" stops being a property of the type and becomes a property of
//! whichever `#[serde(skip_serializing)]` attribute someone remembered.
//!
//! So the wire shape is always a separate type in [`super::dto`], reached
//! through a `From`/`TryFrom`. Adding a column therefore cannot leak it; it has
//! to be copied into a DTO by hand first.
//!
//! [`row_structs_are_not_wire_types`](tests::row_structs_are_not_wire_types)
//! scans this file and fails if a derive here names either trait, and
//! `no_storage_row_struct_is_published_unless_it_is_the_wire_shape` in
//! `server::routes::openapi` fails if one of these names reaches the published
//! document.

use chrono::NaiveDateTime;

// ============================================================
// Workflow
// ============================================================

#[derive(Debug, Clone, sqlx::FromRow)]
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

// ============================================================
// Channel
// ============================================================

#[derive(Debug, Clone, sqlx::FromRow)]
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

// ============================================================
// Connector
// ============================================================

#[derive(Debug, Clone, sqlx::FromRow)]
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

/// One execution record, read whole. Carries the payloads *and*
/// [`Self::access_token_hash`], so it is only ever fetched for a single trace
/// the caller has already been authorised for — list pages read
/// [`TraceListRow`] instead.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Trace {
    pub id: String,
    /// The channel **name** as it was at execution time — an immutable
    /// snapshot, not a lookup key (D26). It is deliberately kept alongside
    /// `channel_id` and deliberately not refreshed: renaming a channel must
    /// not rewrite the history of what already ran, and a trace has to stay
    /// readable after its channel is deleted. Filter and group by
    /// `channel_id` when you mean "this channel"; read `channel` when you
    /// mean "what it was called then".
    pub channel: String,
    /// Stable identity of the channel that ran, when one was resolved. `None`
    /// for rows written before the column existed.
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
    /// Per-task `dataflow_rs::ExecutionTrace` JSON, captured only when the
    /// channel has `config.tracing.task_details = true`. Workflow authors
    /// can inspect intermediate inputs/outputs for each task to debug
    /// pipelines without re-running them in dry-run.
    pub task_trace_json: Option<String>,
    /// SHA-256 hash of the capability token returned with the async 202 (R12).
    /// A credential verifier: compared against a presented token, never shown.
    /// It is safe to hold here precisely because this struct cannot be
    /// serialized — see the module rule.
    pub access_token_hash: Option<String>,
}

/// List-view projection over `traces`. Deliberately omits `input_json`,
/// `result_json`, `task_trace_json` and `access_token_hash` (D27): a trace
/// listing would otherwise carry every caller's request body, the full engine
/// message, and one credential verifier per row — all of it read out of the
/// database and into process memory for rows the response never shows.
///
/// The list query names these columns explicitly, so `SELECT *` cannot quietly
/// widen the page again.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TraceListRow {
    pub id: String,
    /// See [`Trace::channel`] — the name at execution time, not a key.
    pub channel: String,
    pub channel_id: Option<String>,
    pub mode: String,
    pub status: String,
    pub error_message: Option<String>,
    pub duration_ms: Option<f64>,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// ============================================================
// Trace DLQ
// ============================================================

#[derive(Debug, Clone, sqlx::FromRow)]
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

/// List-view projection over `trace_dlq`. Deliberately omits `payload_json` /
/// `metadata_json`: a DLQ listing would otherwise dump every failed request's
/// body — and, on rows written before S10, its headers — into one response.
/// Payloads are served one at a time by `get_by_id`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TraceDlqSummary {
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

// ============================================================
// Audit log
// ============================================================

#[derive(Debug, Clone, sqlx::FromRow)]
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
mod tests {
    /// The module rule, checked against the module (D27, D28).
    ///
    /// There is no way to write "`T` does *not* implement `Serialize`" as a
    /// bound in stable Rust, so the rule is enforced against the source: every
    /// `#[derive(...)]` in this file must name neither trait. A row struct that
    /// picks one up — and with it the ability to put a column like
    /// `access_token_hash` on the wire by accident — fails here.
    #[test]
    fn row_structs_are_not_wire_types() {
        const SOURCE: &str = include_str!("rows.rs");
        let mut rest = SOURCE;
        while let Some(start) = rest.find("#[derive(") {
            rest = &rest[start + "#[derive(".len()..];
            let end = rest.find(")]").expect("unterminated #[derive(...)]");
            let derives = &rest[..end];
            for banned in ["Serialize", "ToSchema"] {
                assert!(
                    !derives.contains(banned),
                    "`{banned}` derived on a row struct in models/rows.rs \
                     (derive list: `{derives}`). A row struct is a picture of a table, \
                     not a wire shape — give it a DTO in models/dto.rs and a \
                     `From`/`TryFrom` instead."
                );
            }
            rest = &rest[end..];
        }
    }
}
