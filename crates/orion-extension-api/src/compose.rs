//! The registration surface an edition composes a node with, and the one
//! error composing it can fail with.

use std::sync::Arc;

use crate::admission::{EditionTier, PackageVerifier};
use crate::channel::ChannelDriver;
use crate::cluster::SharedStateStore;
use crate::contract::ImplementationKind;
use crate::identity::BackendKey;
use crate::pool::PoolEntry;
use crate::provider::ProviderBackend;

/// Where an edition registers what its node serves. `orion-server`'s
/// `CompositionBuilder` implements it, so an edition's registration code
/// can be written against this crate alone:
///
/// ```
/// use std::sync::Arc;
/// use orion_extension_api::{EditionTier, Registrar};
///
/// pub fn register(node: &mut dyn Registrar) {
///     node.edition(EditionTier::Standard);
///     // The edition's own types:
///     // node.channel_driver(Arc::new(QueueChannelDriver));
///     // node.provider_backend(Arc::new(DocumentStoreProvider::new()));
///     // node.package_verifier(Arc::new(LicencedPackageVerifier::new()));
///     // node.cluster_store(Arc::new(RedisSharedStore::lazy(&settings)));
/// #   let _ = Arc::new(());
/// }
/// ```
///
/// Nothing is checked as it is registered: the node keys everything when it
/// builds the composition, and the first duplicate is a [`CompositionError`].
pub trait Registrar {
    /// The edition the node reports.
    fn edition(&mut self, edition: EditionTier);

    /// A channel driver, keyed by its protocol id (case-insensitively).
    fn channel_driver(&mut self, driver: Arc<dyn ChannelDriver>);

    /// A connector provider backend, keyed by `(kind, driver_id)`.
    fn provider_backend(&mut self, backend: Arc<dyn ProviderBackend>);

    /// A connection-pool cache, keyed by its `cache_id`
    /// (`PoolEntry::of(Arc::new(cache))`).
    fn resource_pool(&mut self, pool: PoolEntry);

    /// The package verifier, replacing the one already composed.
    fn package_verifier(&mut self, verifier: Arc<dyn PackageVerifier>);

    /// The cluster's shared hot-path store, in place of the state
    /// database's. One per node: a second is a [`CompositionError`].
    ///
    /// Registration cannot wait on the network, so hand over a client that
    /// connects lazily. The node pings it at boot, bounded by
    /// `engine.health_check_timeout_secs`, and refuses to start when it does
    /// not answer; `/readyz` then reports it as its `cluster_store`
    /// component. With `cluster.enabled = false` it is unused, and the node
    /// says so at startup.
    fn cluster_store(&mut self, store: Arc<dyn SharedStateStore>);
}

/// A second backend under a `(kind, driver_id)` that already has one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a provider backend is already registered as ({kind}, '{driver_id}')")]
pub struct DuplicateProvider {
    pub kind: ImplementationKind,
    pub driver_id: String,
}

impl DuplicateProvider {
    /// Construct from a typed [`BackendKey`].
    pub fn from_key(key: &BackendKey) -> Self {
        Self {
            kind: key.kind(),
            driver_id: key.driver_id().to_string(),
        }
    }
}

/// A schema a composed extension declares that is outside the
/// `orion-json-schema/1` profile: it would refuse every call that needs it,
/// so the node refuses to build instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
#[error(
    "provider '{driver_id}' declares a {slot} schema for '{operation}' outside \
     orion-json-schema/1: {code} at {location}{}",
    pointer_suffix(.schema_pointer)
)]
pub struct InvalidSchema {
    /// The provider's `driver_id`.
    pub driver_id: String,
    /// The operation whose schema it is (as declared).
    pub operation: String,
    /// `input` or `output`.
    pub slot: String,
    /// The profile's finding code (`SCHEMA_INVALID`,
    /// `SCHEMA_UNSUPPORTED_KEYWORD`, …).
    pub code: String,
    /// Where the profile found it.
    pub location: String,
    /// The JSON pointer inside the schema, when the finding has one.
    pub schema_pointer: String,
}

impl InvalidSchema {
    /// A finding about `driver_id`'s `operation`/`slot` schema.
    pub fn new(
        driver_id: impl Into<String>,
        operation: impl Into<String>,
        slot: impl Into<String>,
        code: impl Into<String>,
        location: impl Into<String>,
        schema_pointer: impl Into<String>,
    ) -> Self {
        Self {
            driver_id: driver_id.into(),
            operation: operation.into(),
            slot: slot.into(),
            code: code.into(),
            location: location.into(),
            schema_pointer: schema_pointer.into(),
        }
    }
}

fn pointer_suffix(pointer: &str) -> String {
    if pointer.is_empty() {
        String::new()
    } else {
        format!(" (schema pointer {pointer})")
    }
}

/// Why a composition does not build. Most variants are a second
/// registration of something the composition already holds: every extension
/// point is keyed (a channel protocol, a provider's `(kind, driver_id)`, a
/// pool's `cache_id`) or single (the cluster store), and a node serves one
/// of each. The rest are a
/// registered extension that could never serve.
///
/// Non-exhaustive: new extension points add variants.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CompositionError {
    /// Two channel drivers for one protocol (compared case-insensitively).
    #[error("composition: a channel driver for protocol '{0}' is already registered")]
    DuplicateChannelDriver(String),
    /// Two provider backends under one `(kind, driver_id)`.
    #[error("composition: {0}")]
    DuplicateProvider(DuplicateProvider),
    /// Two connection-pool caches with one `cache_id`.
    #[error("composition: a connection-pool cache '{0}' is already registered")]
    DuplicatePool(String),
    /// A provider backend's `describe()` declares a schema outside
    /// `orion-json-schema/1`.
    #[error("composition: {0}")]
    InvalidProviderSchema(Box<InvalidSchema>),
    /// A channel driver's `transport_schema()` is outside
    /// `orion-json-schema/1`.
    #[error(
        "composition: the '{protocol}' channel driver's transport schema is outside \
         orion-json-schema/1: {code} at {location}"
    )]
    InvalidTransportSchema {
        /// The driver's protocol id.
        protocol: String,
        /// The profile's finding code.
        code: String,
        /// Where the profile found it (with the schema pointer, when it has one).
        location: String,
    },
    /// A channel driver declares capabilities that cannot be served together
    /// (an HTTP hook beside its own listener, an HTTP hook with no route).
    #[error("composition: the '{protocol}' channel driver cannot be served: {reason}")]
    InvalidChannelDriver {
        /// The driver's protocol id.
        protocol: String,
        /// Which capabilities conflict.
        reason: String,
    },
    /// Two cluster shared-state stores.
    #[error(
        "composition: a cluster store '{existing}' is already registered; \
         '{added}' cannot be registered beside it"
    )]
    DuplicateClusterStore {
        /// The first store's `name()`.
        existing: String,
        /// The second store's `name()`.
        added: String,
    },
}
