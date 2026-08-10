//! Domain vocabulary: the small closed sets that both a row and a DTO spell,
//! plus the string constants the SQL and the wire agree on.
//!
//! The definitions moved to the shared `orion-api` crate so the CLI reads the
//! same vocabulary the server writes; they are re-exported here under their
//! pre-1.0 paths. What stays behind is [`parse_json_field`] — the row → DTO
//! bridge — because it speaks [`OrionError`], which is server-internal.

use crate::errors::OrionError;

pub use orion_api::enums::{
    CHANNEL_TYPE_ASYNC, CHANNEL_TYPE_SYNC, ChannelProtocol, ChannelType, EntityStatus,
    PackageState, STATUS_ACTIVE, STATUS_ARCHIVED, STATUS_DRAFT, TRACE_MODE_ASYNC, TRACE_MODE_SYNC,
    TRACE_STATUS_COMPLETED, TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
    VALID_CHANNEL_TYPES,
};

/// Parse a JSON string column into a typed value, wrapping failures with
/// enough context to name the offending row and field.
///
/// Lives here rather than in [`super::dto`] because it is the shared bridge
/// between a row's `*_json` `String` and a DTO's `Value`: every
/// row → DTO conversion goes through it.
pub(super) fn parse_json_field<T: serde::de::DeserializeOwned>(
    json_str: &str,
    entity_type: &str,
    entity_id: &str,
    field_name: &str,
) -> Result<T, OrionError> {
    serde_json::from_str(json_str).map_err(|e| OrionError::Internal {
        context: format!("Corrupt JSON in {entity_type} {entity_id} {field_name}"),
        source: Some(Box::new(e)),
    })
}
