//! Internals the integration and storage test binaries drive directly.
//!
//! Not an API. The repository contract runs the deployment store against
//! each database backend from its own test binary (the backend is pinned
//! per process), and the integration tests build manifests and admit
//! provider backends the way a node does; neither can be a unit test.
//! Everything an edition implements is in `orion-extension-api`; what it
//! hands to [`crate::run`] is [`crate::extensions::CompositionBuilder`].

pub use crate::extensions::admission::Admission;
pub use crate::extensions::authoring::scaffold_provider_package;
pub use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
pub use crate::extensions::deployment_store::{
    ArchiveEntryInput, ArtifactState, CanonicalManifest, DeploymentReceipt, EntityKind, EntityRef,
    ImmutableDeploymentStore, ManifestMember, Rollback,
};
pub use crate::extensions::foundation::{
    DRIVER_HTTP, DRIVER_MEMORY_CACHE, DRIVER_MYSQL, DRIVER_POSTGRESQL, DRIVER_SQLITE,
    MysqlProviderBackend, SqlLimits, builtin_provider_digest,
};
pub use crate::extensions::owned_native_work::NativeWorkScheduler;
pub use crate::extensions::package_identity::canonical_bytes;
pub use crate::extensions::provider_backend::ProviderRegistry;
pub use crate::extensions::source_runner::{SourceLeaseCoordinator, SourcePartitionKey};
