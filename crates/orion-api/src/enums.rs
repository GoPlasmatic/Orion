//! Domain vocabulary: the small closed sets that both sides of the wire
//! spell, plus the string constants the SQL and the wire agree on.
//!
//! Everything here is a *value*, never a record. Nothing in this file knows
//! about a table or an endpoint.

use serde::{Deserialize, Serialize};

// -- Entity status --

/// The lifecycle status strings as they appear on the wire and in status
/// columns. [`EntityStatus::as_str`] returns exactly these, so matching on a
/// constant and matching on the enum can never disagree.
pub const STATUS_DRAFT: &str = "draft";
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_ARCHIVED: &str = "archived";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum EntityStatus {
    Draft,
    Active,
    Archived,
}

impl EntityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => STATUS_DRAFT,
            Self::Active => STATUS_ACTIVE,
            Self::Archived => STATUS_ARCHIVED,
        }
    }
}

impl std::fmt::Display for EntityStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// -- Channel type constants --
pub const CHANNEL_TYPE_SYNC: &str = "sync";
pub const CHANNEL_TYPE_ASYNC: &str = "async";
pub const VALID_CHANNEL_TYPES: [&str; 2] = [CHANNEL_TYPE_SYNC, CHANNEL_TYPE_ASYNC];

// -- Channel type enum (used by typed DTOs) --
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ChannelType {
    Sync,
    Async,
}

impl ChannelType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sync => CHANNEL_TYPE_SYNC,
            Self::Async => CHANNEL_TYPE_ASYNC,
        }
    }
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Strict: the wire value is the lowercase word the type serializes to.
impl<'de> Deserialize<'de> for ChannelType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            CHANNEL_TYPE_SYNC => Ok(Self::Sync),
            CHANNEL_TYPE_ASYNC => Ok(Self::Async),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[CHANNEL_TYPE_SYNC, CHANNEL_TYPE_ASYNC],
            )),
        }
    }
}

/// A channel's `protocol`: the name of the channel driver that serves it, taken
/// exactly as written. Deliberately a string rather than an enum — the set of
/// protocols is whatever drivers the node's composition registers, so an
/// unknown one (including a differently-cased `REST`) is refused by
/// validation, by field, naming the registered ones, not by this parse.
pub fn deserialize_protocol<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    String::deserialize(deserializer)
}

// -- Trace status constants --
pub const TRACE_STATUS_PENDING: &str = "pending";
pub const TRACE_STATUS_RUNNING: &str = "running";
pub const TRACE_STATUS_COMPLETED: &str = "completed";
pub const TRACE_STATUS_FAILED: &str = "failed";

// -- Trace mode constants --
// The transport a traced message arrived on, as written to `traces.mode` and
// accepted by `GET /api/v2/admin/traces?mode=`. The field is an open string on
// the wire, so a client must tolerate a value it does not know — but these are
// the ones the runtime writes.
pub const TRACE_MODE_SYNC: &str = "sync";
pub const TRACE_MODE_ASYNC: &str = "async";
/// One occurrence of a cron channel's schedule. The row's `input_json` is the
/// channel's authored `transport_config.payload`, and the occurrence ledger
/// (`GET /api/v2/admin/cron/occurrences`) carries the scheduling detail the
/// trace itself does not: the scheduled instant, the attempt and the singleton.
pub const TRACE_MODE_CRON: &str = "cron";

#[cfg(test)]
mod tests {
    use super::*;

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
        let draft: EntityStatus = serde_json::from_str(r#""draft""#).expect("test");
        assert_eq!(draft, EntityStatus::Draft);
        let active: EntityStatus = serde_json::from_str(r#""active""#).expect("test");
        assert_eq!(active, EntityStatus::Active);
        let archived: EntityStatus = serde_json::from_str(r#""archived""#).expect("test");
        assert_eq!(archived, EntityStatus::Archived);
        // Invalid status should fail
        assert!(serde_json::from_str::<EntityStatus>(r#""pending""#).is_err());
    }

    #[test]
    fn status_constants_match_the_enum() {
        assert_eq!(EntityStatus::Draft.as_str(), STATUS_DRAFT);
        assert_eq!(EntityStatus::Active.as_str(), STATUS_ACTIVE);
        assert_eq!(EntityStatus::Archived.as_str(), STATUS_ARCHIVED);
    }

    #[test]
    fn test_valid_channel_types() {
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_SYNC));
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_ASYNC));
    }

    #[test]
    fn test_channel_type_is_exact_lowercase() {
        let lower: ChannelType = serde_json::from_str(r#""sync""#).expect("test");
        assert_eq!(lower, ChannelType::Sync);
        assert!(serde_json::from_str::<ChannelType>(r#""SYNC""#).is_err());
        assert!(serde_json::from_str::<ChannelType>(r#""Async""#).is_err());
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
        assert_eq!(TRACE_MODE_CRON, "cron");
    }
}
