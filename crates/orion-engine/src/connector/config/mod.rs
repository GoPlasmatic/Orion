mod cache;
mod db;
mod guards;
mod http;
#[cfg(test)]
mod tests;

pub use cache::{CacheConnectorConfig, VALID_CACHE_BACKENDS};
pub use db::{DbConnectorConfig, DbDriver};
pub use guards::{
    CacheOperationGates, DialectGuards, HttpOperationGates, OperationGates, VALID_HTTP_METHODS,
};
pub use http::{
    AuthConfig, HttpConnectorConfig, OAuth2ClientAuth, OAuth2Config, OAuth2Grant, RetryConfig,
};

use serde::{Deserialize, Serialize};

/// A parsed connector configuration.
///
/// `PartialEq` runs the whole tree and is load-bearing, not a convenience:
/// [`crate::connector::ConnectorRegistry::load`] compares the map it
/// just built against the one it holds and only advances
/// `config_generation` when they differ, which is what keeps a deployment
/// that touched no connector from invalidating every channel's cached runtime.
/// Every field of every variant therefore has to take part — a field
/// left out of the comparison would be a config change the channel loader
/// never notices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorConfig {
    Http(HttpConnectorConfig),
    Db(DbConnectorConfig),
    Cache(CacheConnectorConfig),
}

/// One parse of a connector variant's struct — the hook
/// [`ConnectorConfig::parse_variant`] types each variant through.
pub trait VariantParse {
    type Error;
    fn parse<T: serde::de::DeserializeOwned>(
        &self,
        value: &serde_json::Value,
    ) -> Result<T, Self::Error>;
}

impl ConnectorConfig {
    /// Type `value` as `connector_type`'s variant, through `parser`.
    ///
    /// The same result as deserializing the enum with `type` injected, except
    /// that the variant's struct is deserialized directly. That matters to a
    /// parse that needs error *paths* — the authoring parse that stands a
    /// placeholder in for a reference, the load parse that coerces a resolved
    /// boolean: serde buffers an internally tagged document before handing it
    /// to the variant, and the path is lost there. A `type` key in `value` is
    /// ignored, as the tag is.
    pub fn parse_variant<P: VariantParse>(
        connector_type: ConnectorType,
        value: &serde_json::Value,
        parser: &P,
    ) -> Result<Self, P::Error> {
        let mut untagged = value.clone();
        if let Some(obj) = untagged.as_object_mut() {
            obj.remove("type");
        }
        let value = &untagged;
        Ok(match connector_type {
            ConnectorType::Http => Self::Http(parser.parse(value)?),
            ConnectorType::Db => Self::Db(parser.parse(value)?),
            ConnectorType::Cache => Self::Cache(parser.parse(value)?),
        })
    }

    /// The wire `type` this config is. The stored `connectors.connector_type`
    /// column says the same thing, but the registry holds parsed configs — this
    /// is how a caller with only the parsed form (F52's activation-time type
    /// check) asks what it got.
    pub fn connector_type(&self) -> ConnectorType {
        match self {
            ConnectorConfig::Http(_) => ConnectorType::Http,
            ConnectorConfig::Db(_) => ConnectorType::Db,
            ConnectorConfig::Cache(_) => ConnectorType::Cache,
        }
    }

    /// The db operation gates. The other connector types carry gates of their
    /// own shape — [`CacheOperationGates`] and [`HttpOperationGates`] —
    /// reached through their typed configs,
    /// because a `methods` list on a database or a `raw_write` flag on a cache
    /// would be a field nothing reads (the defect F22 closed).
    pub fn operation_gates(&self) -> Option<&OperationGates> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.operations),
            _ => None,
        }
    }

    /// The portable-dialect guards for connectors that carry them (db).
    pub fn dialect_guards(&self) -> Option<&DialectGuards> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.dialect),
            _ => None,
        }
    }
}

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] = &["http", "db", "cache"];

/// Typed enum for the connector `type` field on create/update requests.
/// Wire format is lowercase ("http", "db", "cache");
/// deserialization is case-insensitive so "HTTP" or "Cache" also parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Http,
    Db,
    Cache,
}

impl ConnectorType {
    /// Every connector type, for the callers that must answer a question for
    /// all of them. [`PoolSlot::ALL`](super::kind::PoolSlot::ALL) exists next
    /// door for the same reason: the per-kind vocabulary was written out by
    /// hand in four places, and a gate struct had already fallen out of one
    /// of them — the gate-key drift guard, which enumerated five types and so
    /// could not see the sixth.
    ///
    /// A `match` over this with no wildcard arm is the pattern to reach for:
    /// it turns a new variant into a compile error rather than a check that
    /// silently stops covering it.
    pub const ALL: &'static [ConnectorType] =
        &[ConnectorType::Http, ConnectorType::Db, ConnectorType::Cache];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Db => "db",
            Self::Cache => "cache",
        }
    }

    /// The keys this type's `operations` object may carry — the gate
    /// vocabulary its handlers actually read.
    ///
    /// Connector configs deliberately do not use `deny_unknown_fields` (a
    /// 0.3.x row carrying a since-removed field has to keep loading — see
    /// `legacy_configs_with_removed_fields_still_load`), and every gate struct
    /// is `#[serde(default)]`. So `{"operations": {"writes": false}}` on a
    /// cache would deserialize into a *fully open* gate: accepted, stored, and
    /// silently doing nothing. For a control whose promise is "regardless of
    /// what any workflow asks for", that is the worst possible failure mode,
    /// so `validation::connectors` checks the keys against this list at the
    /// admin door. Stored configs are never re-validated, so nothing that
    /// already loads stops loading.
    pub fn operation_gate_keys(self) -> &'static [&'static str] {
        match self {
            Self::Db => &["read", "insert", "update", "delete", "upsert", "raw_write"],
            Self::Cache => &["read", "write"],
            Self::Http => &["methods"],
        }
    }

    /// The type a stored `connector_type` column names — exactly as the
    /// config enum's tag matches it, so the two cannot disagree on a row.
    pub fn from_stored(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|t| t.as_str() == s).copied()
    }
}

impl std::fmt::Display for ConnectorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ConnectorType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?.to_ascii_lowercase();
        // Matched against `ALL` rather than a second hand-written arm list:
        // the spelling each variant answers to is `as_str`'s to decide, and a
        // new variant should not need a line here to become parseable.
        Self::ALL
            .iter()
            .find(|t| t.as_str() == s)
            .copied()
            .ok_or_else(|| serde::de::Error::unknown_variant(&s, VALID_CONNECTOR_TYPES))
    }
}
