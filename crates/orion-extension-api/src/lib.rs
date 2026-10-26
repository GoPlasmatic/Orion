//! The extension surface of the Orion runtime.
//!
//! An edition adds to what a node serves by implementing the traits here and
//! registering them on the node's composition through [`Registrar`]
//! (`orion-server`'s `CompositionBuilder` implements it; its binary hands the
//! builder to `orion::run`):
//!
//! | Extension point | Trait | Keyed by |
//! |---|---|---|
//! | A channel protocol | [`ChannelDriver`] | its protocol id |
//! | A connector provider | [`ProviderBackend`] (+ [`SourceConnectorFacet`]) | `(kind, driver_id)` |
//! | A provider's pool cache | [`ConnectionPoolCache`] | its `cache_id` |
//! | Package admission | [`PackageVerifier`] | one per node |
//!
//! A connector call answers `Result<`[`ConnectorOutput`]`, `[`ConnectorError`]`>`
//! and is handed an [`AttemptContext`], a read-only view of the run the node
//! admitted.
//!
//! # Vocabulary
//!
//! - **Connector**: the authored form, a connector file in a definition set,
//!   which `compile` turns into a connection.
//! - **Connection**: an immutable, numbered revision of a connector's
//!   configuration, named by [`ConnectionId`] and [`Revision`] and pinned in
//!   a running call as a [`ConnectionReference`].
//! - **Provider**: what a connection runs on, pinned as a [`ProviderRef`]
//!   (publisher, [`ProviderId`], version, [`Digest`]): a driver, or an
//!   uploaded provider package.
//! - **Driver**: a provider compiled into the binary (`org.orion.http`, …).
//!   It has no package; a connection names it by its built-in digest.
//! - **Backend**: the [`ProviderBackend`] a node dispatches a provider's
//!   operations to, registered under its [`ImplementationKind`] and backend
//!   id: a `rust` backend's driver id, or a `workflow`/`wasm` package's
//!   digest.
//! - **`target_id`**: the `connector_call` input naming that backend id;
//!   with none, the connection's provider decides it.
//!
//! A `rust` backend is keyed by driver id, not by digest: its code is the
//! binary, so one backend serves every package version that names its id.
//! A `workflow` or `wasm` backend's id *is* its digest.

mod admission;
mod attempt;
mod channel;
mod compose;
mod contract;
mod identity;
mod pool;
mod provider;

pub use admission::{
    AdmissionRejection, AdmissionVerdict, EditionTier, PackageAdmissionRequest, PackageVerifier,
};
pub use attempt::{AttemptContext, AttemptScope};
pub use channel::{ChannelDriver, ChannelDriverCapabilities, ChannelProtocolFields};
pub use compose::{CompositionError, DuplicateProvider, Registrar};
pub use contract::{
    ConnectorError, ConnectorErrorClass, ConnectorOutput, EffectStatus, ErrorOrigin,
    ImplementationKind, OriginKind, RetryAdvice, codes,
};
pub use identity::{
    ConnectionId, ConnectionReference, EffectiveConnectionIdentity, ProviderId, ProviderRef,
    Revision,
};
pub use pool::{ConnectionPoolCache, PoolEntry};
pub use provider::{
    ProviderBackend, SourceBatchItem, SourceConnectorFacet, SourcePollBatch, SourceSettlement,
};

/// The wire types a contract shares with `orion-api`. The vocabularies
/// ([`ImplementationKind`], [`ConnectorErrorClass`], [`EffectStatus`],
/// [`EditionTier`]) are `orion_api::v2`'s too, re-exported above.
pub use orion_api::{Digest, DigestError, FieldError};

/// The attribute the async traits here are declared with; implement them
/// with it too.
pub use async_trait::async_trait;
