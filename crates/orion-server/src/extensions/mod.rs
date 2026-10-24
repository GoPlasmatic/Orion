//! Open extension seams, composition root and owned context contracts (`V2-11.1`, `V2-05.1`).
//!
//! # Architecture & Dependency Direction
//!
//! This module defines the public registration and runtime contracts around Orion's
//! core library target so the public repository stays focused on the **Community Edition**
//! while **Enterprise Edition** extends it cleanly from a separate private workspace:
//!
//! - **Channels (100% Native Rust via [`ChannelDriver`])**:
//!   Community ships the foundational ingress and scheduler drivers (`http`, `rest`,
//!   `cron`, `source`) in [`ChannelDriverRegistry::community`]. Enterprise registers
//!   additional native Rust drivers (`graphql`, `kafka`, `sse`) via
//!   [`CompositionRoot::with_channel_driver`] without adding variants to a closed
//!   `ChannelProtocol` enum.
//! - **Connectors (Unified 3-Backend Architecture via [`ProviderBackend`])**:
//!   Supports Direct Rust (`ImplementationKind::Rust`), Workflow-Group composite
//!   packages (`ImplementationKind::Workflow`), and sandboxed Wasm components
//!   (`ImplementationKind::Wasm`) under one invocation contract ([`ConnectorResult`]),
//!   owned context ([`RunContext`], [`Invocation`], [`AttemptContext`]), and
//!   optional [`SourceConnectorFacet`] (`open` / `poll` / `settle` / `close`).
//! - **Resource Pooling (Dynamic via [`ResourcePoolRegistry`] & [`VersionedSessionPool`])**:
//!   Direct Rust providers register their own [`ConnectionPoolCache`] instances and
//!   participate in per-connector and blanket cluster-resync eviction without
//!   requiring a closed `PoolSlot` enum variant.
//! - **Package Admission & Security Lease ([`AdmissionPolicy`])**:
//!   Community uses [`CommunityAdmissionPolicy`]; Enterprise injects its signed-package
//!   and entitlement verifier via [`CompositionRoot::with_admission_policy`].

// `ConnectorError` intentionally carries structured `ErrorOrigin`, `EffectStatus`,
// `RetryAdvice`, and optional JSON `details` inline per the v2 connector contract (`V2-10.1`).
#![allow(clippy::result_large_err)]

pub mod admission;
pub mod authoring;
pub mod channel_driver;
pub mod community_assembly;
pub mod conformance_harness;
pub mod context;
pub mod contract;
pub mod deployment_store;
pub mod foundation;
pub mod migration;
pub mod owned_native_work;
pub mod package_identity;
pub mod provider_backend;
pub mod reconciler;
pub mod resource_pool;
pub mod schema_profile;
pub mod source_runner;
pub mod wasm_provider;
pub mod workflow_provider;

use std::sync::Arc;

pub use admission::{
    AdmissionPolicy, AdmissionRejection, AdmissionVerdict, CommunityAdmissionPolicy,
    PackageAdmissionRequest,
};
pub use channel_driver::{
    ChannelDriver, ChannelDriverCapabilities, ChannelDriverRegistry, ChannelProtocolFields,
    CronChannelDriver, HttpChannelDriver, KafkaChannelDriver, RestChannelDriver,
    SourceChannelDriver,
};
pub use community_assembly::{
    BoundaryViolation, COMMUNITY_CHANNEL_PROTOCOLS, CommunityAssemblyReport,
    verify_community_assembly,
};
pub use conformance_harness::{ConformanceObservation, execute_conformance_case};
pub use context::{
    AdmittedRun, AttemptContext, AuthorityGrant, BudgetCounters, BudgetLimits, BudgetScope,
    CancellationScope, CapabilityGrant, CapabilityHandle, CapabilityTable, ChargeVector,
    HostJobRegistry, Invocation, OwnedHostJob, Reservation, ResourceKind, RunContext, WorkLease,
};
pub use contract::{
    ConnectionReference, ConnectorError, ConnectorErrorClass, ConnectorOutput, ConnectorResult,
    DeploymentIdentity, EffectStatus, EffectiveConnectionIdentity, ErrorOrigin, ImplementationKind,
    OperationEffect, OperationRetryPolicy, OriginKind, ProviderReference, RetryAdvice, codes,
};
pub use deployment_store::{
    ArchiveEntryInput, ArtifactObjectRow, ArtifactState, CandidatePlan, CanonicalManifest,
    ConnectionRevisionRow, DeploymentError, DeploymentReceipt, DeploymentRevisionRow, EntityKind,
    EntityRef, ImmutableDeploymentStore, ManifestMember, ProviderAdmissionRow, ProviderPackageRow,
    ProviderRef, RolloutInterval, validate_rollout_partition,
};
pub use foundation::{
    COMMUNITY_RUST_DRIVERS, DRIVER_HTTP, DRIVER_MEMORY_CACHE, DRIVER_POSTGRESQL, DRIVER_SQLITE,
    FoundationOptions, HttpProviderBackend, MemoryCacheProviderBackend, PartitionedMemoryCache,
    PostgresProviderBackend, SqlLimits, SqliteProviderBackend,
    register_community_foundation_providers,
};
pub use migration::{
    ENTERPRISE_DRIVER_ELASTICSEARCH, ENTERPRISE_DRIVER_KAFKA, ENTERPRISE_DRIVER_MONGODB,
    ENTERPRISE_DRIVER_MYSQL, ENTERPRISE_DRIVER_REDIS, ENTERPRISE_DRIVER_SMTP,
    ENTERPRISE_DRIVER_STORAGE, EditionTier, MigrationFinding, ResolvedLegacyConnector,
    ResolvedLegacyFunction, check_channel_protocol_availability, check_connector_availability,
    map_legacy_connector, map_legacy_function,
};
pub use owned_native_work::{
    NativeJobInfo, NativeJobState, NativeWorkError, NativeWorkScheduler, OwnedPermit,
    ShutdownDrainReport, SingleFlightLoader,
};
pub use package_identity::{
    DeclaredLockDependency, MAX_PACKAGE_FILE_BYTES, MAX_PACKAGE_FILES, MAX_PACKAGE_TOTAL_BYTES,
    PackageIdentityFinding, SIGNATURE_DOMAIN_PREFIX, VerifiedArtifactMetadata, build_inventory,
    canonical_bytes, check_dependency_lock, sha256_digest, sha256_hex, signature_signing_bytes,
    validate_payload_path, verify_inventory,
};
pub use provider_backend::{
    BoxProviderFuture, ProviderBackend, ProviderRegistry, SourceBatchItem, SourceConnectorFacet,
    SourcePollBatch, SourceSettlement,
};
pub use reconciler::{
    CandidateBuildIssue, ConvergenceReport, ConvergenceTarget, DEFAULT_CONVERGENCE_TIMEOUT_MS,
    DEFAULT_SECURITY_LEASE_MS, MAX_BACKOFF_MS, MIN_BACKOFF_MS, NodeIdentity, NodeReconciler,
    QuarantinedEntity, RuntimeNodeStatus, compute_reconcile_backoff_ms,
};
pub use resource_pool::{
    BoxPoolFuture, ConnectionPoolCache, ResourcePoolRegistry, SessionHandle, VersionedSessionPool,
};
pub use schema_profile::{
    CompiledSchemaBundle, DIALECT_URI, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, PROFILE_ID,
    SchemaProfileFinding, SchemaProfileLimits, check_manifest_bundle, parse_json_pointer,
    parse_strict_json, resolve_pointer, validate_safe_package_path,
};
pub use source_runner::{
    ContiguousCheckpointLedger, ItemSettlementState, SourceChannelRunner, SourceCoordinationError,
    SourceLeaseCoordinator, SourcePartitionClaim, SourcePartitionKey, SourceRunnerLimits,
};
pub use wasm_provider::{
    CONNECTOR_WIT_CONTRACT, VerifiedWasmComponent, WasmComponentCache, WasmGuestExecutor,
    WasmImportProfile, WasmInvocationStore, WasmProviderBackend, WasmStoreLimits, WasmVerifyError,
};
pub use workflow_provider::{
    BindingSlotSpec, CompiledWorkflowProviderGraph, SubChannelDef, TransitiveCapabilityReport,
    WorkflowGraphCompileError, WorkflowGroupManifest, WorkflowGroupProviderBackend,
    WorkflowGroupStep, WorkflowNodeDef, WorkflowSourceEntries,
};

/// Composition root assembling the node's channel drivers, connector provider
/// backends, admission policy, extensible connection-pool caches, immutable
/// deployment store, node reconciler, and owned native work scheduler (`V2-11.1`).
#[derive(Clone)]
pub struct CompositionRoot {
    pub channels: ChannelDriverRegistry,
    pub providers: ProviderRegistry,
    pub admission: Arc<dyn AdmissionPolicy>,
    pub pools: ResourcePoolRegistry,
    pub deployments: ImmutableDeploymentStore,
    pub reconciler: NodeReconciler,
    pub native_scheduler: NativeWorkScheduler,
}

impl std::fmt::Debug for CompositionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositionRoot")
            .field("channels", &self.channels)
            .field("providers", &self.providers)
            .field("admission_policy", &self.admission.policy_id())
            .field("pools", &self.pools)
            .finish()
    }
}

impl Default for CompositionRoot {
    fn default() -> Self {
        Self::community()
    }
}

impl CompositionRoot {
    /// Construct the default Community Edition composition root (`V2-11.1`, `V2-14`).
    ///
    /// Registers:
    /// - Foundational native Rust channel drivers (`rest`, `http`, `cron`, `source`)
    /// - Foundational Direct Rust connector providers (`org.orion.http`,
    ///   `org.orion.postgresql`, `org.orion.sqlite`, `org.orion.memory-cache`) and
    ///   their version-fenced session/cache pools
    /// - [`CommunityAdmissionPolicy`]
    pub fn community() -> Self {
        Self::community_with(&foundation::FoundationOptions::default())
    }

    /// [`Self::community`], with the foundation providers bounded by `options`
    /// (the node's shared HTTP client and its configured caps).
    pub fn community_with(options: &foundation::FoundationOptions) -> Self {
        let providers = ProviderRegistry::new();
        let pools = ResourcePoolRegistry::new();
        let deployments = ImmutableDeploymentStore::default();
        foundation::register_community_foundation_providers_with(
            &providers,
            &pools,
            Some(deployments.clone()),
            options,
        );
        let admission: Arc<dyn AdmissionPolicy> = Arc::new(CommunityAdmissionPolicy::new());
        providers.set_admission_policy(admission.clone());
        providers.set_deployment_store(deployments.clone());
        let reconciler = NodeReconciler::new(deployments.clone(), "node-local-1", "boot-1");
        Self {
            channels: ChannelDriverRegistry::community(),
            providers,
            admission,
            pools,
            deployments,
            reconciler,
            native_scheduler: NativeWorkScheduler::new(16),
        }
    }

    /// Register an additional native Rust [`ChannelDriver`] (e.g. `graphql`, `sse`, `kafka`).
    pub fn with_channel_driver(self, driver: Arc<dyn ChannelDriver>) -> Self {
        self.channels.register(driver);
        self
    }

    /// Register a connector [`ProviderBackend`] (`rust`, `workflow`, or `wasm`).
    pub fn with_provider_backend(self, backend: Arc<dyn ProviderBackend>) -> Self {
        self.providers.register(backend);
        self
    }

    /// Replace the package and runtime [`AdmissionPolicy`] (e.g. for Enterprise Edition).
    pub fn with_admission_policy(mut self, policy: Arc<dyn AdmissionPolicy>) -> Self {
        self.providers.set_admission_policy(policy.clone());
        self.admission = policy;
        self
    }

    /// Override the node reconciler's [`NodeIdentity`] (`node_id` and `boot_id`)
    /// with the running instance's configured cluster/node identity.
    pub fn with_node_identity(
        mut self,
        node_id: impl Into<String>,
        boot_id: impl Into<String>,
    ) -> Self {
        self.reconciler = NodeReconciler::new(self.deployments.clone(), node_id, boot_id);
        self
    }

    /// Register a typed [`ConnectionPoolCache`] so it participates in per-connector
    /// and blanket cluster-resync pool eviction.
    pub fn with_resource_pool<C>(self, cache: Arc<C>) -> Self
    where
        C: ConnectionPoolCache + 'static,
    {
        self.pools.register(cache);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn composition_root_wires_channels_providers_admission_and_pools() {
        let pool = Arc::new(VersionedSessionPool::<String>::new("orion.sqlite", 4));
        let root = CompositionRoot::community().with_resource_pool(pool.clone());

        assert_eq!(root.admission.policy_id(), "orion.community.admission/1");
        assert_eq!(
            root.channels.protocol_ids(),
            vec!["cron", "http", "rest", "source"]
        );
        // 3 foundation stateful pools (postgresql, sqlite, memory-cache) + 1 custom pool registered above = 4.
        assert_eq!(root.pools.len(), 4);
        assert!(root.pools.get::<VersionedSessionPool<String>>().is_some());
        assert_eq!(
            root.providers.registered_drivers(),
            vec![
                (ImplementationKind::Rust, DRIVER_HTTP.to_string()),
                (ImplementationKind::Rust, DRIVER_MEMORY_CACHE.to_string()),
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

        root.pools.evict_connector("local-sqlite").await;
        assert_eq!(pool.cached_count(), 0);

        // Verify CommunityAdmissionPolicy rejects enterprise-tier packages on Community root.
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
            tier: Some("enterprise".to_string()),
            signature: None,
        };
        let err = root
            .admission
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
            tier: Some("community".to_string()),
            signature: None,
        };
        let verdict = root
            .admission
            .verify_package(&comm_req)
            .expect("community package admitted");
        assert_eq!(verdict.provider.provider_id, "orion.http");
        let _ = json!({});
    }
}
