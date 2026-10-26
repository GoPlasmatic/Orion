//! The registration surface an edition composes a node with, and the one
//! error composing it can fail with.

use std::sync::Arc;

use crate::admission::{EditionTier, PackageVerifier};
use crate::channel::ChannelDriver;
use crate::contract::ImplementationKind;
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
///     node.edition(EditionTier::Enterprise);
///     // node.channel_driver(Arc::new(KafkaChannelDriver));
///     // node.provider_backend(Arc::new(MongoProvider::new()));
///     // node.package_verifier(Arc::new(EnterprisePackageVerifier::new()));
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
}

/// A second backend under a `(kind, driver_id)` that already has one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a provider backend is already registered as ({kind}, '{driver_id}')")]
pub struct DuplicateProvider {
    pub kind: ImplementationKind,
    pub driver_id: String,
}

/// A second registration of something the composition already holds. Every
/// extension point is keyed (a channel protocol, a provider's
/// `(kind, driver_id)`, a pool's `cache_id`), and a node serves one of each.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
}
