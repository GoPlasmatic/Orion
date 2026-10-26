//! The identity vocabulary of providers and connections.
//!
//! - [`Digest`]: a content digest, `sha256:<64 lowercase hex>`, checked once
//!   where the text comes in and trusted after (defined in `orion-api`, so
//!   the CLI checks a digest the same way, and re-exported at this crate's root).
//! - [`ProviderId`] and [`ConnectionId`]: the two names a call is made
//!   under. They are not validated here (their rules belong to whoever
//!   accepts them); the types exist so one is never passed for the other.
//! - [`Revision`]: an immutable entity or connection revision, numbered
//!   from 1. A deployment *head* is not one: `0` there means "no head", so
//!   deployment revisions stay `u64`.
//! - [`ProviderRef`]: one provider package, pinned — publisher, id, version
//!   and digest. The same type in a deployment manifest and in a running
//!   call.
//! - [`ConnectionReference`]: one connection revision and the provider it
//!   runs on; [`EffectiveConnectionIdentity`] is what partitions pools and
//!   sessions.
//!
//! All of them serialize as their plain value, so moving a field from
//! `String`/`u64` to one of these does not change a wire format.

use std::borrow::Borrow;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

use orion_api::Digest;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_string())
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&String> for $name {
            fn from(id: &String) -> Self {
                Self(id.clone())
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

string_id! {
    /// A provider's namespaced id (`org.orion.http`, `acme.billing`): the
    /// `id` of its package manifest, and the driver id a `rust` backend
    /// registers under.
    ProviderId
}

string_id! {
    /// A connection's stable logical id, shared by all of its revisions.
    ConnectionId
}

/// An immutable revision of a deployment entity or a connection: numbered
/// from 1, so `0` cannot be spelled. Serialized as the plain integer; a `0`
/// is refused when it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(NonZeroU64);

impl Revision {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// `n` as a revision, or `None` for `0`.
    pub const fn new(n: u64) -> Option<Self> {
        match NonZeroU64::new(n) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// The revision after this one.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl From<Revision> for u64 {
    fn from(r: Revision) -> Self {
        r.get()
    }
}

impl TryFrom<u64> for Revision {
    type Error = std::num::TryFromIntError;

    fn try_from(n: u64) -> Result<Self, Self::Error> {
        NonZeroU64::try_from(n).map(Self)
    }
}

impl PartialEq<u64> for Revision {
    fn eq(&self, other: &u64) -> bool {
        self.get() == *other
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A provider package, pinned: who published it, its id, its semantic
/// version, and the digest of its bytes (`V2-03.1`).
///
/// One type for every place a provider is named: a deployment manifest's
/// `providers`, an admission request, and the connection a running call is
/// bound to. A built-in driver has no uploaded package; its reference is
/// a synthetic one the node gives it (its built-in digest).
/// The field order is the canonical sort order of a manifest's providers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderRef {
    pub publisher: String,
    pub provider_id: ProviderId,
    /// Exact semantic version string.
    pub version: String,
    pub digest: Digest,
}

/// Pinned connection reference within a compiled runtime generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConnectionReference {
    pub connection_id: ConnectionId,
    pub revision: Revision,
    /// Pinned provider package artifact.
    pub provider: ProviderRef,
}

/// Effective identity used to partition stateful sessions and connection pools (`V2-05.4`, `CTX-08`).
///
/// Two invocations share a session pool entry only when their provider artifact,
/// connection ID, connection revision, and resolved credential hash all match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectiveConnectionIdentity {
    pub provider_digest: Digest,
    pub connection_id: ConnectionId,
    pub revision: Revision,
    /// SHA-256 digest of where the connection's secrets come from (never of
    /// a secret), or empty when it has none.
    pub secret_hash: String,
}

impl EffectiveConnectionIdentity {
    /// Stable capacity key shared across revisions of the same logical connection
    /// so rotating credentials or updating config does not multiply max pool ceilings (`CTX-08`).
    pub fn stable_capacity_key(&self) -> (&str, &str) {
        (&self.provider_digest, &self.connection_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_is_never_zero_on_the_wire_either() {
        assert_eq!(Revision::new(0), None);
        assert_eq!(Revision::FIRST.get(), 1);
        assert_eq!(Revision::FIRST.next().get(), 2);
        assert_eq!(serde_json::to_value(Revision::FIRST).unwrap(), 1);
        assert!(serde_json::from_value::<Revision>(serde_json::json!(0)).is_err());
        assert_eq!(
            serde_json::from_value::<Revision>(serde_json::json!(7)).unwrap(),
            7
        );
    }

    #[test]
    fn ids_serialize_as_their_string() {
        let id = ConnectionId::from("orders-db");
        assert_eq!(serde_json::to_value(&id).unwrap(), "orders-db");
        assert_eq!(id, "orders-db");
        assert_eq!(&*id, "orders-db");
    }
}
