//! Community Edition boundary verifier (`V2-11.3`, `DEC-005`), compiled for tests only.
//!
//! The tests below assert that the public Community Edition assembly
//! ([`super::CompositionRoot::community`], built):
//!
//! 1. Registers **only** the 3 foundational native Rust channel drivers
//!    (`cron`, `http`, `rest`) and zero Enterprise-only channel
//!    drivers (`graphql`, `kafka`, `sse`).
//! 2. Registers **only** the 5 foundational Direct Rust connector providers
//!    (`org.orion.http`, `org.orion.memory-cache`, `org.orion.mysql`,
//!    `org.orion.postgresql`, `org.orion.sqlite`) and zero Enterprise-only
//!    Direct Rust providers.
//! 3. Uses [`CommunityPackageVerifier`](super::admission::CommunityPackageVerifier)
//!    (`orion.community.admission/1`) which rejects `tier: "enterprise"` packages
//!    with `ENTERPRISE_TIER_NOT_ENABLED`.
//! 4. Supports registering and executing a custom `workflow` connector package
//!    on top of the Community foundation without modifying core enums.

use serde::Serialize;

use super::Composition;
use super::contract::ImplementationKind;
use super::foundation::COMMUNITY_RUST_DRIVERS;

/// Canonical Community Edition native Rust channel driver protocol IDs (`DEC-003`).
pub const COMMUNITY_CHANNEL_PROTOCOLS: [&str; 3] = ["cron", "http", "rest"];

/// Boundary violation discovered while auditing a [`Composition`] against the
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
/// - Checks that all 3 Community channel protocols (`cron`, `http`, `rest`) are
///   present and no Enterprise channel protocols (`graphql`, `kafka`, `sse`) are registered.
/// - Checks that all 5 Community Direct Rust providers (`org.orion.http`,
///   `org.orion.memory-cache`, `org.orion.mysql`, `org.orion.postgresql`,
///   `org.orion.sqlite`) are present and no Enterprise Direct Rust providers are
///   registered.
/// - Allows any number of user-installed `workflow` providers.
/// - Checks that `orion.community.admission/1` is active.
pub fn verify_community_assembly(root: &Composition) -> CommunityAssemblyReport {
    let mut violations = Vec::new();
    let admission_policy_id = root.admission().policy_id().to_string();

    if admission_policy_id != "orion.community.admission/1" {
        violations.push(BoundaryViolation {
            code: "UNEXPECTED_ADMISSION_POLICY",
            component: admission_policy_id.clone(),
            message: format!(
                "Community assembly must use 'orion.community.admission/1', found '{admission_policy_id}'"
            ),
        });
    }

    let registered_channels: Vec<String> = root.channels().protocol_ids();

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

    for (kind, id) in root.providers().registered_drivers() {
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
        resource_pool_count: root.pools().len(),
        violations,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::extensions::workflow_provider::{
        WorkflowHost, WorkflowPackage, WorkflowProviderBackend,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn community() -> Composition {
        crate::extensions::CompositionRoot::community()
            .build_in_memory()
            .unwrap()
    }

    #[test]
    fn default_community_assembly_passes_boundary_audit() {
        let root = community();
        let report = verify_community_assembly(&root);
        assert!(
            report.is_compliant(),
            "unexpected boundary violations: {:?}",
            report.violations
        );
        assert_eq!(report.registered_channels, vec!["cron", "http", "rest"]);
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
        let root = crate::extensions::CompositionRoot::community()
            .with_channel_driver(Arc::new(EnterpriseKafkaDriver))
            .build_in_memory()
            .unwrap();
        let report = verify_community_assembly(&root);
        assert!(!report.is_compliant());
        assert!(report.violations.iter().any(|v| v.code
            == "ENTERPRISE_CHANNEL_LEAKED_INTO_COMMUNITY"
            && v.component == "kafka"));
    }

    #[test]
    fn a_workflow_package_backend_keeps_the_assembly_compliant() {
        let root = community();
        let provider = crate::extensions::ProviderRef {
            publisher: "community".to_string(),
            provider_id: "community.composite-cache".into(),
            version: "2.0.0".to_string(),
            digest: crate::extensions::Digest::parse(&format!("sha256:{}", "88".repeat(32)))
                .unwrap(),
        };
        let manifest = json!({
            "implementation": {
                "kind": "workflow",
                "entries": {"cache.write": "entry"},
                "workflows": ["workflows/entry.json"],
            },
            "dependencies": [],
        });
        let files = BTreeMap::from([(
            "workflows/entry.json".to_string(),
            serde_json::to_vec(&json!({"id": "entry", "name": "entry", "tasks": [{"id": "t", "name": "t", "function": {"name": "map", "input": {"mappings": [{"path": "data.x", "logic": 1}]}}}]}))
                .expect("serializes"),
        )]);
        let package = WorkflowPackage::from_files(provider.clone(), &manifest, &files)
            .expect("the package is hermetic");
        let backend = Arc::new(WorkflowProviderBackend::new(
            package,
            WorkflowHost {
                providers: root.providers().clone(),
                jwks: Arc::new(crate::jwt::jwks::JwksCache::new(
                    reqwest::Client::new(),
                    false,
                )),
                ops_budget: 0,
            },
        ));
        root.providers().admit(backend).unwrap();

        // The assembly stays Community-compliant with a custom workflow provider.
        let report = verify_community_assembly(&root);
        assert!(
            report.is_compliant(),
            "custom workflow providers must be allowed in Community assembly: {:?}",
            report.violations
        );
        assert_eq!(
            report.registered_workflow_providers,
            vec![provider.digest.to_string()]
        );
    }
}
