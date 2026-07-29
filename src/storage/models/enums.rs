//! Domain vocabulary: the small closed sets that both a row and a DTO spell,
//! plus the string constants the SQL and the wire agree on.
//!
//! Everything here is a *value*, never a record. Nothing in this file knows
//! about a table or an endpoint.

use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

// -- Entity status enum --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EntityStatus {
    Draft,
    Active,
    Archived,
}

impl EntityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Archived => "archived",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
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

// Custom case-insensitive deserialize so v0.1 lowercase wire values still
// parse while broader inputs like "SYNC" or "Async" now succeed too —
// strictly additive on the v0.1 acceptance set.
impl<'de> Deserialize<'de> for ChannelType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            CHANNEL_TYPE_SYNC => Ok(Self::Sync),
            CHANNEL_TYPE_ASYNC => Ok(Self::Async),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[CHANNEL_TYPE_SYNC, CHANNEL_TYPE_ASYNC],
            )),
        }
    }
}

// -- Channel protocol enum --
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChannelProtocol {
    Rest,
    Http,
    Kafka,
}

impl ChannelProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Http => "http",
            Self::Kafka => "kafka",
        }
    }
}

impl std::fmt::Display for ChannelProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Case-insensitive deserialize matching ChannelType's behavior.
impl<'de> Deserialize<'de> for ChannelProtocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "rest" => Ok(Self::Rest),
            "http" => Ok(Self::Http),
            "kafka" => Ok(Self::Kafka),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["rest", "http", "kafka"],
            )),
        }
    }
}

// -- Trace status constants --
pub const TRACE_STATUS_PENDING: &str = "pending";
pub const TRACE_STATUS_RUNNING: &str = "running";
pub const TRACE_STATUS_COMPLETED: &str = "completed";
pub const TRACE_STATUS_FAILED: &str = "failed";

// -- Trace mode constants --
pub const TRACE_MODE_SYNC: &str = "sync";
pub const TRACE_MODE_ASYNC: &str = "async";

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
    serde_json::from_str(json_str).map_err(|e| OrionError::InternalSource {
        context: format!("Corrupt JSON in {entity_type} {entity_id} {field_name}"),
        source: Box::new(e),
    })
}

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
    fn test_valid_channel_types() {
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_SYNC));
        assert!(VALID_CHANNEL_TYPES.contains(&CHANNEL_TYPE_ASYNC));
    }

    #[test]
    fn test_channel_protocol_as_str() {
        assert_eq!(ChannelProtocol::Rest.as_str(), "rest");
        assert_eq!(ChannelProtocol::Http.as_str(), "http");
        assert_eq!(ChannelProtocol::Kafka.as_str(), "kafka");
    }

    #[test]
    fn test_channel_protocol_display() {
        assert_eq!(ChannelProtocol::Rest.to_string(), "rest");
        assert_eq!(ChannelProtocol::Http.to_string(), "http");
        assert_eq!(ChannelProtocol::Kafka.to_string(), "kafka");
    }

    #[test]
    fn test_channel_protocol_serde_roundtrip() {
        let rest: ChannelProtocol = serde_json::from_str(r#""rest""#).expect("test");
        assert_eq!(rest, ChannelProtocol::Rest);
        let http: ChannelProtocol = serde_json::from_str(r#""http""#).expect("test");
        assert_eq!(http, ChannelProtocol::Http);
        let kafka: ChannelProtocol = serde_json::from_str(r#""kafka""#).expect("test");
        assert_eq!(kafka, ChannelProtocol::Kafka);
        // Invalid protocol should fail
        assert!(serde_json::from_str::<ChannelProtocol>(r#""grpc""#).is_err());
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
    }
}
