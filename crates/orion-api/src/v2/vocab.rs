//! The closed vocabularies the v2 contract carries.
//!
//! Each is an enum on the wire, not a string: a value outside it is refused
//! on the way in, and a client reading an answer gets a variant it can
//! match. Adding a variant is a change to the wire, which
//! [`API_VERSION`](super::API_VERSION) moves for: an older client would
//! refuse an answer carrying it.
//!
//! The two vocabularies only the wire reads — [`PackageArtifactState`] and
//! [`EditionFindingCode`] — are `#[non_exhaustive]`, so a client's match on
//! them already has an arm for a later variant. The five the runtime itself
//! dispatches on are deliberately **exhaustive**: the provider registry
//! keeps one table per [`ImplementationKind`], `connector_call` maps every
//! [`ConnectorErrorClass`] to a task error class, the reconciler builds
//! every [`EntityKind`], and an edition's provider backends construct and
//! match [`EffectStatus`] and [`EditionTier`]. A new variant there needs a
//! decision at each of those sites, and every crate that matches them ships
//! at the workspace's one version, so a compile error at each site is the
//! signal wanted — `#[non_exhaustive]` would turn it into a silent wildcard
//! arm.
//!
//! None of them carries an `Unknown(String)` fallback. Each set is fixed by
//! the contract that defines it (the three connector implementation kinds,
//! the two editions, the normalised error classes and effect states, the
//! entity kinds a manifest deploys), and version skew is negotiated through
//! `api_version` rather than absorbed by a catch-all. A vocabulary that does
//! grow on its own — a channel protocol, a driver id, a schema profile, an
//! advisory's `check` id — stays a `String`, because it is an identifier,
//! not a closed set.
//!
//! `orion-extension-api` re-exports [`ImplementationKind`],
//! [`ConnectorErrorClass`], [`EffectStatus`] and [`EditionTier`]: the
//! extension traits name them and so does the wire, and this is their one
//! definition.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! names {
    ($ty:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The name the wire spells.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }

            /// The variant named `name`, or `None`.
            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = UnknownName;

            fn from_str(name: &str) -> Result<Self, Self::Err> {
                Self::parse(name).ok_or_else(|| UnknownName {
                    name: name.to_string(),
                    expected: Self::ALL.iter().map(|v| v.as_str()).collect(),
                })
            }
        }
    };
}

/// A name that is not one of a vocabulary's ([`FromStr`] on each enum here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownName {
    pub name: String,
    pub expected: Vec<&'static str>,
}

impl fmt::Display for UnknownName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "'{}' is not one of: {}",
            self.name,
            self.expected.join(", ")
        )
    }
}

impl std::error::Error for UnknownName {}

/// The three connector implementation kinds.
///
/// Channels are not among them: a channel is a native ingress or scheduler
/// driver, whereas a connector provider may be a Rust driver, a
/// workflow-group composite, or a Wasm component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ImplementationKind {
    /// A native Rust provider compiled into the node (`orion-rust-provider/1`).
    Rust,
    /// A composite of private workflows, sub-channels and connector slots.
    Workflow,
    /// A sandboxed WebAssembly component (`orion:connector@2.0.0`).
    Wasm,
}

names!(ImplementationKind {
    Rust => "rust",
    Workflow => "workflow",
    Wasm => "wasm",
});

/// The normalised class of a connector failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConnectorErrorClass {
    InvalidInput,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Unavailable,
    Deadline,
    Cancelled,
    ProviderFailure,
    ContractViolation,
}

names!(ConnectorErrorClass {
    InvalidInput => "invalid_input",
    Unauthorized => "unauthorized",
    Forbidden => "forbidden",
    NotFound => "not_found",
    Conflict => "conflict",
    RateLimited => "rate_limited",
    Unavailable => "unavailable",
    Deadline => "deadline",
    Cancelled => "cancelled",
    ProviderFailure => "provider_failure",
    ContractViolation => "contract_violation",
});

/// What the host observed of an external effect.
///
/// Ordered by uncertainty: once an attempt or a child records
/// [`EffectStatus::Unknown`], a later local pre-send rejection cannot
/// downgrade the logical invocation back to [`EffectStatus::NotStarted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    /// No relevant external operation started across this logical invocation and descendants.
    NotStarted,
    /// Relevant external operations settled (including an observed downstream rejection).
    Completed,
    /// At least one external operation may have taken effect without conclusive settlement evidence.
    Unknown,
}

names!(EffectStatus {
    NotStarted => "not_started",
    Completed => "completed",
    Unknown => "unknown",
});

impl EffectStatus {
    /// Combine prior invocation effect evidence with a newly observed attempt/child status.
    ///
    /// - `Unknown` dominates everything unless an explicit idempotent settlement overrides it.
    /// - `NotStarted` before any prior activity remains `NotStarted`; after `Completed` or
    ///   `Unknown`, a pre-send failure preserves the earlier evidence.
    pub const fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Completed, _) | (_, Self::Completed) => Self::Completed,
            (Self::NotStarted, Self::NotStarted) => Self::NotStarted,
        }
    }
}

/// An edition, and the distribution tier a provider package declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum EditionTier {
    Community,
    Enterprise,
}

names!(EditionTier {
    Community => "community",
    Enterprise => "enterprise",
});

/// The kind of a deployment member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Workflow,
    Channel,
    Connection,
    Source,
    Model,
    Plugin,
}

names!(EntityKind {
    Workflow => "workflow",
    Channel => "channel",
    Connection => "connection",
    Source => "source",
    Model => "model",
    Plugin => "plugin",
});

/// Where a stored provider package's bytes are in their lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PackageArtifactState {
    /// Stored and servable.
    Active,
    /// No deployment names it any more; the collector will delete it after
    /// the drain epoch.
    Draining,
    /// The bytes are gone; only the record remains.
    Deleted,
}

names!(PackageArtifactState {
    Active => "active",
    Draining => "draining",
    Deleted => "deleted",
});

/// Why an edition transition preflight refuses a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum EditionFindingCode {
    /// The manifest pins a provider only the Enterprise edition serves.
    EnterpriseProviderInManifest,
}

names!(EditionFindingCode {
    EnterpriseProviderInManifest => "ENTERPRISE_PROVIDER_IN_MANIFEST",
});

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `as_str`, `parse` and serde agree on every name.
    #[test]
    fn every_name_round_trips_through_serde() {
        fn check<T>(all: &[T], name: fn(&T) -> &'static str, parse: fn(&str) -> Option<T>)
        where
            T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
        {
            for v in all {
                let wire = serde_json::to_value(v).unwrap();
                assert_eq!(wire, name(v), "{v:?}");
                assert_eq!(parse(name(v)).as_ref(), Some(v));
                assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), v);
            }
        }
        check(
            ImplementationKind::ALL,
            |v| v.as_str(),
            ImplementationKind::parse,
        );
        check(
            ConnectorErrorClass::ALL,
            |v| v.as_str(),
            ConnectorErrorClass::parse,
        );
        check(EffectStatus::ALL, |v| v.as_str(), EffectStatus::parse);
        check(EditionTier::ALL, |v| v.as_str(), EditionTier::parse);
        check(EntityKind::ALL, |v| v.as_str(), EntityKind::parse);
        check(
            PackageArtifactState::ALL,
            |v| v.as_str(),
            PackageArtifactState::parse,
        );
        check(
            EditionFindingCode::ALL,
            |v| v.as_str(),
            EditionFindingCode::parse,
        );
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_set() {
        assert!(serde_json::from_str::<ImplementationKind>(r#""python""#).is_err());
        let err = "python".parse::<ImplementationKind>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "'python' is not one of: rust, workflow, wasm"
        );
    }
}
