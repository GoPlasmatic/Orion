//! Open extension seams, the composition, and owned context contracts (`V2-11.1`, `V2-05.1`).
//!
//! # Architecture & Dependency Direction
//!
//! This module defines the public registration and runtime contracts around Orion's
//! core library target so the public repository stays focused on the **Community Edition**
//! while **Enterprise Edition** extends it cleanly from a separate private workspace.
//!
//! An edition describes what its node serves with an owned
//! [`CompositionBuilder`] (Community's is [`CompositionRoot::community`]) and
//! hands it to [`crate::run`] / [`crate::bootstrap::assemble`]. Bootstrap
//! creates the node's services (its [`NodeIdentity`], deployment store,
//! [`RevocationLedger`] and [`NativeWorkScheduler`]) and freezes the builder
//! over them into a [`Composition`]: plain maps, private fields, nothing
//! registered after boot. A duplicate registration fails the build.
//!
//! - **Channels (100% Native Rust via [`ChannelDriver`])**:
//!   Community ships the foundational ingress and scheduler drivers (`http`, `rest`,
//!   `cron`, [`community_drivers`]). Enterprise registers additional native
//!   Rust drivers (`graphql`, `kafka`, `sse`) via
//!   [`CompositionBuilder::with_channel_driver`]. The [`ChannelDriverRegistry`]
//!   is the one answer to which protocols exist: the registered driver
//!   validates a channel of its protocol and decides whether it claims a
//!   route, and a protocol with no driver is refused at upload and
//!   quarantined at load.
//! - **Connectors (Unified Backend Architecture via [`ProviderBackend`])**:
//!   Supports Direct Rust (`ImplementationKind::Rust`) and Workflow-Group composite
//!   packages (`ImplementationKind::Workflow`) under one invocation contract ([`ConnectorResult`]),
//!   owned context ([`RunContext`], [`Invocation`], [`AttemptContext`]), and
//!   optional [`SourceConnectorFacet`] (`open` / `poll` / `settle` / `close`).
//!   Composed backends are frozen; packages admitted at runtime live in the
//!   [`ProviderRegistry`]'s separate admitted layer.
//! - **Resource Pooling (via [`ResourcePoolRegistry`] & [`VersionedSessionPool`])**:
//!   Direct Rust providers register their own [`ConnectionPoolCache`] instances
//!   (unique by `cache_id`) without requiring a closed `PoolSlot` enum variant.
//! - **Package Admission ([`PackageVerifier`]) and revocation ([`RevocationLedger`])**:
//!   Community composes [`CommunityPackageVerifier`]; Enterprise injects its
//!   signed-package and entitlement verifier via
//!   [`CompositionBuilder::with_package_verifier`], and its edition via
//!   [`CompositionBuilder::with_edition`]. The revocation ledger and security
//!   lease are core: [`Admission`] checks them whatever the verifier says.

// `ConnectorError` intentionally carries structured `ErrorOrigin`, `EffectStatus`,
// `RetryAdvice`, and optional JSON `details` inline per the v2 connector contract (`V2-10.1`).
#![allow(clippy::result_large_err)]

pub mod admission;
pub mod authoring;
pub mod channel_driver;
#[cfg(test)]
mod community_assembly;
pub mod context;
pub mod contract;
pub mod deployment_store;
pub mod foundation;
pub mod owned_native_work;
pub mod package_identity;
pub mod provider_backend;
pub mod reconciler;
pub mod resource_pool;
pub mod schema_profile;
pub mod source_runner;
pub mod workflow_provider;

use std::sync::Arc;

pub use admission::{
    Admission, AdmissionRejection, AdmissionVerdict, CommunityPackageVerifier, EditionTier,
    PackageAdmissionRequest, PackageVerifier, RevocationLedger,
};
pub use channel_driver::{
    ChannelDriver, ChannelDriverCapabilities, ChannelDriverRegistry, ChannelProtocolFields,
    CronChannelDriver, HttpChannelDriver, PROTOCOL_CRON, PROTOCOL_HTTP, PROTOCOL_REST,
    RestChannelDriver, community_drivers,
};
pub use context::{
    AdmittedRun, AttemptContext, AuthorityGrant, BudgetCounters, BudgetLimits, BudgetScope,
    CancellationScope, ChargeVector, Invocation, Reservation, RunContext, WorkLease,
};
pub use contract::{
    ConnectionReference, ConnectorError, ConnectorErrorClass, ConnectorOutput, ConnectorResult,
    EffectStatus, EffectiveConnectionIdentity, ErrorOrigin, ImplementationKind, OriginKind,
    ProviderReference, RetryAdvice, codes,
};
pub use deployment_store::{
    ArchiveEntryInput, ArtifactObjectRow, ArtifactState, CandidatePlan, CanonicalManifest,
    ConnectionRevisionRow, DeploymentError, DeploymentReceipt, DeploymentRevisionRow, EntityKind,
    EntityRef, ImmutableDeploymentStore, ManifestMember, ProviderAdmissionRow, ProviderPackageRow,
    ProviderRef, RolloutInterval, validate_rollout_partition,
};
pub use foundation::{
    COMMUNITY_RUST_DRIVERS, DRIVER_HTTP, DRIVER_MEMORY_CACHE, DRIVER_MYSQL, DRIVER_POSTGRESQL,
    DRIVER_SQLITE, FoundationOptions, HttpProviderBackend, MemoryCacheProviderBackend,
    MysqlProviderBackend, PartitionedMemoryCache, PostgresProviderBackend, SqlLimits,
    SqliteProviderBackend, register_community_foundation,
};
pub use owned_native_work::{
    NativeJobInfo, NativeJobState, NativeWorkError, NativeWorkScheduler, OwnedPermit,
    ShutdownDrainReport,
};
pub use package_identity::{
    DeclaredLockDependency, MAX_PACKAGE_FILE_BYTES, MAX_PACKAGE_FILES, MAX_PACKAGE_TOTAL_BYTES,
    PackageIdentityFinding, SIGNATURE_DOMAIN_PREFIX, VerifiedArtifactMetadata, build_inventory,
    canonical_bytes, check_dependency_lock, sha256_digest, sha256_hex, signature_signing_bytes,
    validate_payload_path, verify_inventory,
};
pub use provider_backend::{
    DuplicateProvider, ProviderBackend, ProviderRegistry, SourceBatchItem, SourceConnectorFacet,
    SourcePollBatch, SourceSettlement,
};
pub use reconciler::{
    CandidateBuildIssue, ConvergenceReport, ConvergenceTarget, DEFAULT_CONVERGENCE_TIMEOUT_MS,
    DEFAULT_SECURITY_LEASE_MS, MAX_BACKOFF_MS, MIN_BACKOFF_MS, NodeIdentity, NodeReconciler,
    QuarantinedEntity, RuntimeNodeStatus, compute_reconcile_backoff_ms,
};
pub use resource_pool::{
    ConnectionPoolCache, PoolEntry, ResourcePoolRegistry, SessionHandle, VersionedSessionPool,
};
pub use schema_profile::{
    CompiledSchemaBundle, DIALECT_URI, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, PROFILE_ID,
    SchemaProfileFinding, SchemaProfileLimits, check_manifest_bundle, parse_json_pointer,
    resolve_pointer, validate_safe_package_path,
};
pub use source_runner::{
    ContiguousCheckpointLedger, ItemSettlementState, SourceChannelRunner, SourceCoordinationError,
    SourceLeaseCoordinator, SourcePartitionClaim, SourcePartitionKey, SourceRunnerLimits,
};
pub use workflow_provider::{
    PackageDependency, PackageProblem, WorkflowHost, WorkflowPackage, WorkflowProviderBackend,
};

/// A second registration of something the composition already holds. Every
/// extension point is keyed (a channel protocol, a provider's
/// `(kind, driver_id)`, a pool's `cache_id`), and a node serves one of each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositionError {
    /// Two channel drivers for one protocol (compared case-insensitively).
    DuplicateChannelDriver(String),
    /// Two provider backends under one `(kind, driver_id)`.
    DuplicateProvider(DuplicateProvider),
    /// Two connection-pool caches with one `cache_id`.
    DuplicatePool(String),
}

impl std::fmt::Display for CompositionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateChannelDriver(protocol) => write!(
                f,
                "composition: a channel driver for protocol '{protocol}' is already registered"
            ),
            Self::DuplicateProvider(dup) => write!(f, "composition: {dup}"),
            Self::DuplicatePool(id) => write!(
                f,
                "composition: a connection-pool cache '{id}' is already registered"
            ),
        }
    }
}

impl std::error::Error for CompositionError {}

/// What a provider setup ([`CompositionBuilder::with_providers`]) is handed:
/// the node services a backend binds to, and where to put what it builds.
pub struct ProviderRegistrar<'a> {
    deployments: &'a ImmutableDeploymentStore,
    options: &'a FoundationOptions,
    providers: Vec<Arc<dyn ProviderBackend>>,
    pools: Vec<PoolEntry>,
}

impl ProviderRegistrar<'_> {
    /// The node's deployment store: where a backend resolves the governed
    /// connection revision a call runs over.
    pub fn deployments(&self) -> &ImmutableDeploymentStore {
        self.deployments
    }

    /// The node's shared HTTP client and the caps the node's config sets.
    pub fn options(&self) -> &FoundationOptions {
        self.options
    }

    /// Register a provider backend.
    pub fn provider(&mut self, backend: Arc<dyn ProviderBackend>) {
        self.providers.push(backend);
    }

    /// Register a connection-pool cache for per-connector and blanket eviction.
    pub fn pool<C: ConnectionPoolCache + 'static>(&mut self, cache: Arc<C>) {
        self.pools.push(PoolEntry::of(cache));
    }
}

type ProviderSetup = Box<dyn FnOnce(&mut ProviderRegistrar<'_>) + Send>;

/// The node services a composition is built over: bootstrap creates them
/// once per node, before the composition.
pub struct BuildContext {
    /// The node's deployment store, created with the node's identity.
    pub deployments: ImmutableDeploymentStore,
    /// The node's revocation ledger.
    pub ledger: Arc<RevocationLedger>,
    /// The node's shared HTTP client and configured caps.
    pub foundation: FoundationOptions,
}

/// An owned description of what a node serves (`V2-11.1`): its edition, the
/// channel drivers, the connector provider backends, the connection-pool
/// caches and the package verifier. [`Self::build`] turns it into the frozen
/// [`Composition`] the node runs on; nothing registered here can change after
/// that.
///
/// Community's is [`CompositionRoot::community`]. An edition starts from it
/// (or from [`Self::new`]) and adds its own:
///
/// ```no_run
/// # async fn enterprise(config: orion::config::AppConfig) -> Result<(), Box<dyn std::error::Error>> {
/// use orion::extensions::{CompositionRoot, EditionTier};
/// let composition = CompositionRoot::community().with_edition(EditionTier::Enterprise);
/// //  .with_channel_driver(Arc::new(KafkaChannelDriver))
/// //  .with_providers(|r| r.provider(Arc::new(MongoProvider::new(r.deployments().clone()))))
/// //  .with_package_verifier(Arc::new(EnterprisePackageVerifier::new(..)))
/// orion::run(config, composition).await
/// # }
/// ```
///
/// A duplicate registration is an error, reported by [`Self::build`].
pub struct CompositionBuilder {
    edition: EditionTier,
    channels: Vec<Arc<dyn ChannelDriver>>,
    providers: Vec<Arc<dyn ProviderBackend>>,
    provider_setups: Vec<ProviderSetup>,
    pools: Vec<PoolEntry>,
    verifier: Arc<dyn PackageVerifier>,
}

/// The name the Enterprise design docs use for [`CompositionBuilder`]:
/// `CompositionRoot::community()` is Community's builder.
pub type CompositionRoot = CompositionBuilder;

impl std::fmt::Debug for CompositionBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositionBuilder")
            .field("edition", &self.edition)
            .field(
                "channels",
                &self
                    .channels
                    .iter()
                    .map(|d| d.protocol_id())
                    .collect::<Vec<_>>(),
            )
            .field(
                "providers",
                &self
                    .providers
                    .iter()
                    .map(|p| (p.kind(), p.driver_id().to_string()))
                    .collect::<Vec<_>>(),
            )
            .field("provider_setups", &self.provider_setups.len())
            .field("package_verifier", &self.verifier.policy_id())
            .finish()
    }
}

impl CompositionBuilder {
    /// An empty composition for `edition`: no drivers, no providers, no
    /// pools, and Community's package verifier (which refuses
    /// `tier: "enterprise"` packages, so an edition that forgets its own
    /// verifier fails closed).
    pub fn new(edition: EditionTier) -> Self {
        Self {
            edition,
            channels: Vec::new(),
            providers: Vec::new(),
            provider_setups: Vec::new(),
            pools: Vec::new(),
            verifier: Arc::new(CommunityPackageVerifier),
        }
    }

    /// The Community Edition composition (`V2-11.1`, `V2-14`):
    /// - the foundational native Rust channel drivers (`rest`, `http`, `cron`)
    /// - the foundational Direct Rust connector providers (`org.orion.http`,
    ///   `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite`,
    ///   `org.orion.memory-cache`) and their version-fenced session/cache pools
    /// - [`CommunityPackageVerifier`]
    pub fn community() -> Self {
        let mut builder = Self::new(EditionTier::Community);
        for driver in channel_driver::community_drivers() {
            builder = builder.with_channel_driver(driver);
        }
        builder.with_providers(foundation::register_community_foundation)
    }

    /// The edition the node reports.
    pub fn with_edition(mut self, edition: EditionTier) -> Self {
        self.edition = edition;
        self
    }

    /// Register a native Rust [`ChannelDriver`] (e.g. `graphql`, `sse`, `kafka`).
    pub fn with_channel_driver(mut self, driver: Arc<dyn ChannelDriver>) -> Self {
        self.channels.push(driver);
        self
    }

    /// Register a connector [`ProviderBackend`] that needs nothing from the node.
    pub fn with_provider_backend(mut self, backend: Arc<dyn ProviderBackend>) -> Self {
        self.providers.push(backend);
        self
    }

    /// Register the providers (and their pools) `setup` builds once the
    /// node's services exist: a backend that resolves governed connections
    /// needs the node's deployment store, and one that calls out needs its
    /// HTTP client. `setup` runs once, in [`Self::build`].
    pub fn with_providers(
        mut self,
        setup: impl FnOnce(&mut ProviderRegistrar<'_>) + Send + 'static,
    ) -> Self {
        self.provider_setups.push(Box::new(setup));
        self
    }

    /// Register a typed [`ConnectionPoolCache`] so it participates in
    /// per-connector and blanket cluster-resync pool eviction.
    pub fn with_resource_pool<C>(mut self, cache: Arc<C>) -> Self
    where
        C: ConnectionPoolCache + 'static,
    {
        self.pools.push(PoolEntry::of(cache));
        self
    }

    /// Replace the [`PackageVerifier`] (e.g. Enterprise's signed-package and
    /// entitlement verifier). Revocation stays the node's: see [`Admission`].
    pub fn with_package_verifier(mut self, verifier: Arc<dyn PackageVerifier>) -> Self {
        self.verifier = verifier;
        self
    }

    /// Freeze the composition over the node's services: run the provider
    /// setups, then key every driver, provider and pool. The first
    /// duplicate is the error.
    pub fn build(self, node: BuildContext) -> Result<Composition, CompositionError> {
        let Self {
            edition,
            channels,
            providers,
            provider_setups,
            mut pools,
            verifier,
        } = self;
        let BuildContext {
            deployments,
            ledger,
            foundation,
        } = node;

        let channels = ChannelDriverRegistry::new(channels)
            .map_err(CompositionError::DuplicateChannelDriver)?;

        let mut registrar = ProviderRegistrar {
            deployments: &deployments,
            options: &foundation,
            providers,
            pools: Vec::new(),
        };
        for setup in provider_setups {
            setup(&mut registrar);
        }
        let ProviderRegistrar {
            providers: backends,
            pools: setup_pools,
            ..
        } = registrar;
        let mut table = provider_backend::ProviderTable::default();
        for backend in backends {
            table
                .insert(backend)
                .map_err(CompositionError::DuplicateProvider)?;
        }
        pools.extend(setup_pools);
        let pools = ResourcePoolRegistry::new(pools).map_err(CompositionError::DuplicatePool)?;

        Ok(Composition {
            edition,
            channels,
            providers: ProviderRegistry::new(table, deployments, ledger.clone()),
            pools,
            admission: Admission::new(verifier, ledger),
        })
    }

    /// [`Self::build`] over an in-memory deployment store, a fresh ledger and
    /// the default foundation options: for tests and tools that run no node.
    pub fn build_in_memory(self) -> Result<Composition, CompositionError> {
        self.build(BuildContext {
            deployments: ImmutableDeploymentStore::default(),
            ledger: Arc::new(RevocationLedger::new()),
            foundation: FoundationOptions::default(),
        })
    }
}

/// What a node serves, frozen (`V2-11.1`): the edition, the channel drivers,
/// the provider backends, the pool caches and the admission they were
/// composed with, bound to the node's deployment store and revocation
/// ledger. Built by [`CompositionBuilder::build`]; cheap to clone.
///
/// The one runtime-mutable part is the admitted layer of
/// [`ProviderRegistry`] (provider packages the node admits while it runs),
/// which can add backends but never shadow or remove a composed one.
#[derive(Clone)]
pub struct Composition {
    edition: EditionTier,
    channels: ChannelDriverRegistry,
    providers: ProviderRegistry,
    pools: ResourcePoolRegistry,
    admission: Admission,
}

impl std::fmt::Debug for Composition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composition")
            .field("edition", &self.edition)
            .field("channels", &self.channels)
            .field("providers", &self.providers)
            .field("pools", &self.pools)
            .field("admission", &self.admission)
            .finish()
    }
}

impl Composition {
    /// The edition the node reports.
    pub fn edition(&self) -> EditionTier {
        self.edition
    }

    /// The channel drivers: which protocols this node serves.
    pub fn channels(&self) -> &ChannelDriverRegistry {
        &self.channels
    }

    /// The connector provider backends, composed and admitted.
    pub fn providers(&self) -> &ProviderRegistry {
        &self.providers
    }

    /// The connection-pool caches.
    pub fn pools(&self) -> &ResourcePoolRegistry {
        &self.pools
    }

    /// The package verifier over the node's revocation ledger.
    pub fn admission(&self) -> &Admission {
        &self.admission
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_community_composition_wires_channels_providers_admission_and_pools() {
        let pool = Arc::new(VersionedSessionPool::<String>::new("test.sqlite", 4));
        let comp = CompositionRoot::community()
            .with_resource_pool(pool.clone())
            .build_in_memory()
            .expect("community composes");

        assert_eq!(comp.edition(), EditionTier::Community);
        assert_eq!(comp.admission().policy_id(), "orion.community.admission/1");
        assert_eq!(comp.channels().protocol_ids(), vec!["cron", "http", "rest"]);
        // 4 foundation stateful pools (postgresql, mysql, sqlite, memory-cache) + 1 custom pool registered above = 5.
        assert_eq!(comp.pools().len(), 5);
        assert!(
            comp.pools()
                .get_by_id::<VersionedSessionPool<String>>("test.sqlite")
                .is_some()
        );
        assert_eq!(
            comp.providers().registered_drivers(),
            vec![
                (ImplementationKind::Rust, DRIVER_HTTP.to_string()),
                (ImplementationKind::Rust, DRIVER_MEMORY_CACHE.to_string()),
                (ImplementationKind::Rust, DRIVER_MYSQL.to_string()),
                (ImplementationKind::Rust, DRIVER_POSTGRESQL.to_string()),
                (ImplementationKind::Rust, DRIVER_SQLITE.to_string()),
            ]
        );

        // Populate a session in the registered pool and verify eviction through ResourcePoolRegistry.
        let id = EffectiveConnectionIdentity {
            provider_digest:
                "sha256:3333333333333333333333333333333333333333333333333333333333333333"
                    .to_string(),
            connection_id: "local-sqlite".to_string(),
            revision: 1,
            secret_hash: "sha256:0000".to_string(),
        };
        let _handle = pool
            .acquire(&id, || async { Ok("sqlite-conn".to_string()) })
            .await
            .expect("acquired");
        assert_eq!(pool.cached_count(), 1);

        comp.pools().evict_connector("local-sqlite").await;
        assert_eq!(pool.cached_count(), 0);

        // Verify CommunityPackageVerifier rejects enterprise-tier packages on Community root.
        let ent_req = PackageAdmissionRequest {
            provider: ProviderReference {
                provider_id: "orion.kafka".to_string(),
                version: "2.0.0".to_string(),
                package_digest:
                    "sha256:4444444444444444444444444444444444444444444444444444444444444444"
                        .to_string(),
            },
            implementation: ImplementationKind::Rust,
            target_id: "orion.kafka".to_string(),
            tier: crate::extensions::EditionTier::Enterprise,
            signature: None,
        };
        let err = comp
            .admission()
            .verify_package(&ent_req)
            .expect_err("enterprise package must be rejected by Community policy");
        assert_eq!(err.code, "ENTERPRISE_TIER_NOT_ENABLED");

        // Verify valid Community package passes admission.
        let comm_req = PackageAdmissionRequest {
            provider: ProviderReference {
                provider_id: "orion.http".to_string(),
                version: "2.0.0".to_string(),
                package_digest:
                    "sha256:5555555555555555555555555555555555555555555555555555555555555555"
                        .to_string(),
            },
            implementation: ImplementationKind::Rust,
            target_id: "orion.http".to_string(),
            tier: crate::extensions::EditionTier::Community,
            signature: None,
        };
        let verdict = comp
            .admission()
            .verify_package(&comm_req)
            .expect("community package admitted");
        assert_eq!(verdict.provider.provider_id, "orion.http");

        // A revoked digest is refused whatever the verifier says: revocation
        // is the node's ledger, not the edition's verifier.
        comp.admission()
            .ledger()
            .revoke_digest(&comm_req.provider.package_digest);
        let err = comp
            .admission()
            .verify_package(&comm_req)
            .expect_err("revoked digest");
        assert_eq!(err.code, "PROVIDER_REVOKED");
    }

    /// An edition's verifier that admits everything still cannot admit a
    /// revoked package.
    struct AdmitAll;

    impl PackageVerifier for AdmitAll {
        fn policy_id(&self) -> &'static str {
            "test.admit-all/1"
        }

        fn verify_package(
            &self,
            request: &PackageAdmissionRequest,
        ) -> Result<AdmissionVerdict, AdmissionRejection> {
            Ok(AdmissionVerdict {
                provider: request.provider.clone(),
                implementation: request.implementation,
                policy_id: self.policy_id().to_string(),
            })
        }
    }

    #[test]
    fn an_edition_injects_its_edition_and_verifier() {
        let comp = CompositionRoot::community()
            .with_edition(EditionTier::Enterprise)
            .with_package_verifier(Arc::new(AdmitAll))
            .build_in_memory()
            .expect("composes");
        assert_eq!(comp.edition(), EditionTier::Enterprise);
        assert_eq!(comp.admission().policy_id(), "test.admit-all/1");
        let req = PackageAdmissionRequest {
            provider: ProviderReference {
                provider_id: "com.example.premium".to_string(),
                version: "1.0.0".to_string(),
                package_digest: format!("sha256:{}", "ab".repeat(32)),
            },
            implementation: ImplementationKind::Rust,
            target_id: "com.example.premium".to_string(),
            tier: EditionTier::Enterprise,
            signature: None,
        };
        assert!(comp.admission().verify_package(&req).is_ok());
        comp.admission()
            .ledger()
            .revoke_digest(&req.provider.package_digest);
        assert_eq!(
            comp.admission().verify_package(&req).unwrap_err().code,
            "PROVIDER_REVOKED"
        );
    }

    #[test]
    fn a_duplicate_registration_is_a_build_error() {
        let dup_driver = CompositionRoot::community()
            .with_channel_driver(Arc::new(CronChannelDriver))
            .build_in_memory()
            .unwrap_err();
        assert_eq!(
            dup_driver,
            CompositionError::DuplicateChannelDriver("cron".to_string())
        );

        let dup_provider = CompositionRoot::community()
            .with_provider_backend(Arc::new(MemoryCacheProviderBackend::default()))
            .build_in_memory()
            .unwrap_err();
        assert_eq!(
            dup_provider,
            CompositionError::DuplicateProvider(DuplicateProvider {
                kind: ImplementationKind::Rust,
                driver_id: DRIVER_MEMORY_CACHE.to_string(),
            })
        );

        let dup_pool = CompositionRoot::community()
            .with_resource_pool(Arc::new(PartitionedMemoryCache::new(1)))
            .build_in_memory()
            .unwrap_err();
        assert_eq!(
            dup_pool,
            CompositionError::DuplicatePool(DRIVER_MEMORY_CACHE.to_string())
        );
    }

    #[test]
    fn an_admitted_provider_never_shadows_or_removes_a_composed_one() {
        let comp = CompositionRoot::community()
            .build_in_memory()
            .expect("composes");
        let providers = comp.providers();
        let shadow = providers
            .admit(Arc::new(MemoryCacheProviderBackend::default()))
            .unwrap_err();
        assert_eq!(shadow.driver_id, DRIVER_MEMORY_CACHE);
        assert!(!providers.withdraw(ImplementationKind::Rust, DRIVER_MEMORY_CACHE));
        assert!(
            providers
                .get(ImplementationKind::Rust, DRIVER_MEMORY_CACHE)
                .is_some()
        );
    }
}
