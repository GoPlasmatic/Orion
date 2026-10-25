//! Community Edition boundary verifier (`V2-11.3`, `DEC-005`), compiled for tests only.
//!
//! The tests below assert that the public Community Edition assembly
//! ([`CompositionRoot::community`]):
//!
//! 1. Registers **only** the 4 foundational native Rust channel drivers
//!    (`cron`, `http`, `rest`, `source`) and zero Enterprise-only channel
//!    drivers (`graphql`, `kafka`, `sse`).
//! 2. Registers **only** the 5 foundational Direct Rust connector providers
//!    (`org.orion.http`, `org.orion.memory-cache`, `org.orion.mysql`,
//!    `org.orion.postgresql`, `org.orion.sqlite`) and zero Enterprise-only
//!    Direct Rust providers.
//! 3. Uses [`CommunityAdmissionPolicy`](super::admission::CommunityAdmissionPolicy)
//!    (`orion.community.admission/1`) which rejects `tier: "enterprise"` packages
//!    with `ENTERPRISE_TIER_NOT_ENABLED`.
//! 4. Supports registering and executing a custom `workflow` connector package
//!    on top of the Community foundation without modifying core enums.

use serde::Serialize;

use super::CompositionRoot;
use super::contract::ImplementationKind;
use super::foundation::COMMUNITY_RUST_DRIVERS;

/// Canonical Community Edition native Rust channel driver protocol IDs (`DEC-003`).
pub const COMMUNITY_CHANNEL_PROTOCOLS: [&str; 4] = ["cron", "http", "rest", "source"];

/// Boundary violation discovered while auditing a [`CompositionRoot`] against the
/// Community Edition contract (`V2-11.3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoundaryViolation {
    pub code: &'static str,
    pub component: String,
    pub message: String,
}

/// Structured audit report produced by [`verify_community_assembly`] (`V2-11.3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommunityAssemblyReport {
    pub admission_policy_id: String,
    pub registered_channels: Vec<String>,
    pub registered_rust_providers: Vec<String>,
    pub registered_workflow_providers: Vec<String>,
    pub resource_pool_count: usize,
    pub violations: Vec<BoundaryViolation>,
}

impl CommunityAssemblyReport {
    /// Returns `true` when zero boundary violations were detected.
    pub fn is_compliant(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Verify that `root` satisfies the strict Community Edition boundary (`V2-11.3`, `DEC-005`).
///
/// - Checks that all 4 Community channel protocols (`cron`, `http`, `rest`, `source`) are
///   present and no Enterprise channel protocols (`graphql`, `kafka`, `sse`) are registered.
/// - Checks that all 5 Community Direct Rust providers (`org.orion.http`,
///   `org.orion.memory-cache`, `org.orion.mysql`, `org.orion.postgresql`,
///   `org.orion.sqlite`) are present and no Enterprise Direct Rust providers are
///   registered.
/// - Allows any number of user-installed `workflow` providers.
/// - Checks that `orion.community.admission/1` is active.
pub fn verify_community_assembly(root: &CompositionRoot) -> CommunityAssemblyReport {
    let mut violations = Vec::new();
    let admission_policy_id = root.admission.policy_id().to_string();

    if admission_policy_id != "orion.community.admission/1" {
        violations.push(BoundaryViolation {
            code: "UNEXPECTED_ADMISSION_POLICY",
            component: admission_policy_id.clone(),
            message: format!(
                "Community assembly must use 'orion.community.admission/1', found '{admission_policy_id}'"
            ),
        });
    }

    let registered_channels: Vec<String> = root.channels.protocol_ids();

    for expected_channel in COMMUNITY_CHANNEL_PROTOCOLS {
        if !registered_channels.iter().any(|c| c == expected_channel) {
            violations.push(BoundaryViolation {
                code: "MISSING_COMMUNITY_CHANNEL_DRIVER",
                component: expected_channel.to_string(),
                message: format!(
                    "Required Community channel driver '{expected_channel}' is not registered"
                ),
            });
        }
    }

    for ch in &registered_channels {
        if !COMMUNITY_CHANNEL_PROTOCOLS.contains(&ch.as_str()) {
            violations.push(BoundaryViolation {
                code: "ENTERPRISE_CHANNEL_LEAKED_INTO_COMMUNITY",
                component: ch.clone(),
                message: format!(
                    "Non-Community channel driver '{ch}' must not be registered in pure Community assembly"
                ),
            });
        }
    }

    let mut registered_rust_providers = Vec::new();
    let mut registered_workflow_providers = Vec::new();

    for (kind, id) in root.providers.registered_drivers() {
        match kind {
            ImplementationKind::Rust => registered_rust_providers.push(id),
            ImplementationKind::Workflow => registered_workflow_providers.push(id),
            // Community ships no Wasm backend; one registered by an extension
            // is outside this audit.
            ImplementationKind::Wasm => {}
        }
    }

    for &expected_driver in COMMUNITY_RUST_DRIVERS {
        if !registered_rust_providers
            .iter()
            .any(|d| d == expected_driver)
        {
            violations.push(BoundaryViolation {
                code: "MISSING_COMMUNITY_RUST_PROVIDER",
                component: expected_driver.to_string(),
                message: format!(
                    "Required Community Direct Rust provider '{expected_driver}' is not registered"
                ),
            });
        }
    }

    for rust_driver in &registered_rust_providers {
        if !COMMUNITY_RUST_DRIVERS.contains(&rust_driver.as_str()) {
            violations.push(BoundaryViolation {
                code: "UNCATALOGED_DIRECT_RUST_PROVIDER",
                component: rust_driver.clone(),
                message: format!(
                    "Direct Rust provider '{rust_driver}' is not in the DEC-005 Community foundation catalog"
                ),
            });
        }
    }

    CommunityAssemblyReport {
        admission_policy_id,
        registered_channels,
        registered_rust_providers,
        registered_workflow_providers,
        resource_pool_count: root.pools.len(),
        violations,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{
        ConnectionReference, ConnectorResult, EffectiveConnectionIdentity, ProviderReference,
    };
    use crate::extensions::foundation::DRIVER_MEMORY_CACHE;
    use crate::extensions::workflow_provider::{
        BindingSlotSpec, CompiledWorkflowProviderGraph, WorkflowGroupManifest,
        WorkflowGroupProviderBackend, WorkflowGroupStep, WorkflowNodeDef,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn default_community_assembly_passes_boundary_audit() {
        let root = CompositionRoot::community();
        let report = verify_community_assembly(&root);
        assert!(
            report.is_compliant(),
            "unexpected boundary violations: {:?}",
            report.violations
        );
        assert_eq!(
            report.registered_channels,
            vec!["cron", "http", "rest", "source"]
        );
        assert_eq!(
            report.registered_rust_providers,
            vec![
                "org.orion.http",
                "org.orion.memory-cache",
                "org.orion.mysql",
                "org.orion.postgresql",
                "org.orion.sqlite"
            ]
        );
        // 4 stateful foundation pools (postgresql, mysql, sqlite, memory-cache)
        assert_eq!(report.resource_pool_count, 4);
    }

    /// An Enterprise-only protocol, standing in for the native driver the
    /// Enterprise composition root registers.
    struct EnterpriseKafkaDriver;

    impl crate::extensions::ChannelDriver for EnterpriseKafkaDriver {
        fn protocol_id(&self) -> &'static str {
            "kafka"
        }

        fn capabilities(&self) -> crate::extensions::ChannelDriverCapabilities {
            crate::extensions::ChannelDriverCapabilities {
                supports_sync: false,
                supports_async: true,
                registers_http_routes: false,
                supports_oauth2_login: false,
            }
        }

        fn validate_fields(
            &self,
            _fields: &crate::extensions::ChannelProtocolFields<'_>,
        ) -> Vec<crate::errors::FieldError> {
            Vec::new()
        }
    }

    #[test]
    fn detects_enterprise_channel_leak_in_community_audit() {
        let root =
            CompositionRoot::community().with_channel_driver(Arc::new(EnterpriseKafkaDriver));
        let report = verify_community_assembly(&root);
        assert!(!report.is_compliant());
        assert!(report.violations.iter().any(|v| v.code
            == "ENTERPRISE_CHANNEL_LEAKED_INTO_COMMUNITY"
            && v.component == "kafka"));
    }

    #[tokio::test]
    async fn community_assembly_admits_and_runs_a_workflow_package() {
        let root = CompositionRoot::community();

        // 1. Compile and register a custom Workflow-Group connector on the Community root
        //    that delegates to the Community `org.orion.memory-cache` foundation provider.
        let wf_provider_ref = ProviderReference {
            provider_id: "community.composite-cache".to_string(),
            version: "2.0.0".to_string(),
            package_digest:
                "sha256:8888888888888888888888888888888888888888888888888888888888888888"
                    .to_string(),
        };
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "cache_write_entry".to_string(),
            WorkflowNodeDef {
                id: "cache_write_entry".to_string(),
                steps: vec![WorkflowGroupStep::InvokeSlot {
                    binding_slot: "cache_slot".to_string(),
                    operation: "set".to_string(),
                    input: json!({
                        "key": "assembly:test",
                        "value": { "status": "ok" }
                    }),
                    output_key: "cache_result".to_string(),
                }],
            },
        );
        let mut binding_slots = BTreeMap::new();
        binding_slots.insert(
            "cache_slot".to_string(),
            BindingSlotSpec {
                slot: "cache_slot".to_string(),
                provider_id: DRIVER_MEMORY_CACHE.to_string(),
                implementation: ImplementationKind::Rust,
                required_operations: BTreeSet::from(["set".to_string()]),
            },
        );
        let wf_manifest = WorkflowGroupManifest {
            provider: wf_provider_ref.clone(),
            entries: BTreeMap::from([("cache_write".to_string(), "cache_write_entry".to_string())]),
            workflows,
            sub_channels: BTreeMap::new(),
            binding_slots,
            source_entries: None,
        };
        let compiled_wf =
            CompiledWorkflowProviderGraph::compile(wf_manifest).expect("compile workflow group");
        let wf_backend = Arc::new(WorkflowGroupProviderBackend::new(
            compiled_wf,
            root.providers.clone(),
            root.admission.clone(),
        ));

        let root = root.with_provider_backend(wf_backend);

        // The assembly stays Community-compliant with a custom workflow provider.
        let report = verify_community_assembly(&root);
        assert!(
            report.is_compliant(),
            "custom workflow providers must be allowed in Community assembly: {:?}",
            report.violations
        );
        assert_eq!(
            report.registered_workflow_providers,
            vec!["community.composite-cache"]
        );

        // Execute the custom provider through the unified ProviderRegistry.
        let wf_conn = ConnectionReference {
            connection_id: "composite-conn".to_string(),
            revision: 1,
            provider: wf_provider_ref.clone(),
        };
        let cache_conn = ConnectionReference {
            connection_id: "local-cache".to_string(),
            revision: 1,
            provider: ProviderReference {
                provider_id: DRIVER_MEMORY_CACHE.to_string(),
                version: "2.0.0".to_string(),
                package_digest:
                    "sha256:9999999999999999999999999999999999999999999999999999999999999999"
                        .to_string(),
            },
        };
        let authority = AuthorityGrant {
            allowed_operations: BTreeMap::from([(
                "composite-conn".to_string(),
                BTreeSet::from(["cache_write".to_string()]),
            )]),
            declared_dependencies: BTreeMap::from([(
                "cache_slot".to_string(),
                (cache_conn, BTreeSet::from(["set".to_string()])),
            )]),
            allowed_channels: BTreeSet::new(),
        };

        let run = AdmittedRun::admit(
            "run-v2-11-3",
            1,
            Instant::now() + Duration::from_secs(10),
            BudgetLimits::default(),
            authority,
        );

        let inv1 = run
            .context()
            .start_invocation(
                wf_conn,
                EffectiveConnectionIdentity {
                    provider_digest: wf_provider_ref.package_digest.clone(),
                    connection_id: "composite-conn".to_string(),
                    revision: 1,
                    secret_hash: "sha256:0000".to_string(),
                },
                "cache_write",
                None,
                1,
            )
            .expect("wf invocation");
        let res1 = root
            .providers
            .dispatch_attempt(
                root.admission.as_ref(),
                &inv1,
                ImplementationKind::Workflow,
                "community.composite-cache",
                json!({}),
            )
            .await;
        let ConnectorResult::Success { data: data1, .. } = res1 else {
            panic!("expected workflow success, got {res1:?}");
        };
        assert_eq!(data1["cache_result"]["stored"], json!(true));
    }
}
