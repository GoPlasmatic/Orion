//! Unified connector provider backend trait, source facet and registry (`V2-11.1`, `V2-10.1`, `V2-15`).
//!
//! Connector implementations implement [`ProviderBackend`] and register into
//! [`ProviderRegistry`]:
//! - **Direct Rust (`ImplementationKind::Rust`)**: Native Rust drivers (`orion.http`,
//!   `orion.postgresql`, `orion.sqlite`, `orion.memory-cache` in Community;
//!   `orion.mysql`, `orion.mongodb`, `orion.redis`, `orion.elasticsearch`,
//!   `orion.kafka`, `orion.smtp`, `orion.s3` in Enterprise).
//! - **Workflow (`ImplementationKind::Workflow`)**: an uploaded package whose
//!   operations are dataflow workflows, one per operation, run hermetically
//!   ([`crate::extensions::workflow_provider`]) and registered by digest once
//!   admitted.
//!
//! `ImplementationKind::Wasm` is still a package kind a manifest may declare,
//! but no Wasm backend is registered, so a call to one finds no provider.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::admission::AdmissionPolicy;
use super::context::{AttemptContext, Invocation};
use super::contract::{
    ConnectorError, ConnectorOutput, ConnectorResult, EffectStatus, EffectiveConnectionIdentity,
    ImplementationKind, codes,
};

/// Boxed async future used by object-safe [`ProviderBackend`] and [`SourceConnectorFacet`] traits.
pub type BoxProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Source batch item emitted by [`SourceConnectorFacet::poll`] (`V2-15`, §7.2 of `v2-design.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBatchItem {
    pub delivery_id: String,
    pub payload: Value,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

/// Source poll batch returned by [`SourceConnectorFacet::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePollBatch {
    pub items: Vec<SourceBatchItem>,
    pub next_checkpoint: Option<Value>,
}

/// Settlement disposition reported by the native Rust `source` channel runner
/// back to [`SourceConnectorFacet::settle`] after workflow execution completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSettlement {
    Ack,
    NackRetryable,
    DeadLetter,
}

/// Optional source facet (`open` / `poll` / `settle` / `close`) implemented by
/// source-capable connectors of any backend kind (`rust`, `workflow`, `wasm`)
/// and driven by the native Rust `source` channel runner (`V2-15`, §7.2).
pub trait SourceConnectorFacet: Send + Sync {
    /// Open or resume a source cursor from `checkpoint`.
    fn open<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        checkpoint: Option<Value>,
    ) -> BoxProviderFuture<'a, Result<Value, ConnectorError>>;

    /// Poll up to `max_items` records starting from `cursor`.
    fn poll<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: &'a Value,
        max_items: u32,
    ) -> BoxProviderFuture<'a, Result<SourcePollBatch, ConnectorError>>;

    /// Settle a delivered item (`Ack`, `NackRetryable`, or `DeadLetter`).
    fn settle<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        delivery_id: &'a str,
        settlement: SourceSettlement,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>>;

    /// Close the source cursor and release any cursor-specific state.
    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>>;
}

/// Unified connector provider backend trait (`V2-11.1`, `V2-10.1`).
///
/// External Community and Enterprise providers register implementations of this
/// trait without modifying core enums or built-in handler tables.
pub trait ProviderBackend: Send + Sync {
    /// Implementation kind (`Rust`, `Workflow`, or `Wasm`).
    fn kind(&self) -> ImplementationKind;

    /// Driver or adapter identifier (e.g. `"orion.http"`, `"orion.postgresql"`,
    /// `"orion.workflow-host"`).
    fn driver_id(&self) -> &str;

    /// Validate resolved connection configuration against provider-specific invariants.
    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError>;

    /// Execute one attempt of `operation` with validated `input`.
    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>>;

    /// Probe downstream connectivity or readiness under `ctx`.
    fn health_check<'a>(
        &'a self,
        ctx: &'a AttemptContext,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        let _ = ctx;
        Box::pin(async { Ok(()) })
    }

    /// Dispose any provider-held resources for `identity`.
    fn dispose<'a>(
        &'a self,
        identity: &'a EffectiveConnectionIdentity,
    ) -> BoxProviderFuture<'a, ()> {
        let _ = identity;
        Box::pin(async {})
    }

    /// Input keys of `operation` that must be authored as literal strings:
    /// `connector_call` refuses a task whose `input` builds one from the
    /// message, at load. A SQL provider names its statement keys, so a
    /// caller's values reach the database only as bind parameters.
    fn literal_input_keys(&self, _operation: &str) -> &'static [&'static str] {
        &[]
    }

    /// Return the optional [`SourceConnectorFacet`] if this provider supports
    /// `source` channel polling.
    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        None
    }
}

type ProviderBackendMap = BTreeMap<(ImplementationKind, String), Arc<dyn ProviderBackend>>;

/// Open registry of [`ProviderBackend`] implementations (`V2-11.1`).
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    backends: Arc<RwLock<ProviderBackendMap>>,
    admission: Arc<RwLock<Option<Arc<dyn AdmissionPolicy>>>>,
    deployments: Arc<RwLock<Option<super::deployment_store::ImmutableDeploymentStore>>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys = self.registered_drivers();
        f.debug_struct("ProviderRegistry")
            .field("drivers", &keys)
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach an [`AdmissionPolicy`] to this registry so callers such as
    /// `connector_call` always enforce runtime lease and deny-set checks (`TD-25`).
    pub fn set_admission_policy(&self, policy: Arc<dyn AdmissionPolicy>) {
        if let Ok(mut guard) = self.admission.write() {
            *guard = Some(policy);
        }
    }

    /// Return the configured [`AdmissionPolicy`], or a default [`super::admission::CommunityAdmissionPolicy`].
    pub fn admission_policy(&self) -> Arc<dyn AdmissionPolicy> {
        self.admission
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| Arc::new(super::admission::CommunityAdmissionPolicy::new()))
    }

    /// Attach an [`super::deployment_store::ImmutableDeploymentStore`] so callers can resolve
    /// pinned connection revisions and package revocation status (`TD-25`).
    pub fn set_deployment_store(&self, store: super::deployment_store::ImmutableDeploymentStore) {
        if let Ok(mut guard) = self.deployments.write() {
            *guard = Some(store);
        }
    }

    /// Return the attached [`super::deployment_store::ImmutableDeploymentStore`], if any.
    pub fn deployment_store(&self) -> Option<super::deployment_store::ImmutableDeploymentStore> {
        self.deployments.read().ok().and_then(|g| g.clone())
    }

    /// Register a [`ProviderBackend`] by `(kind, driver_id)`.
    pub fn register(&self, backend: Arc<dyn ProviderBackend>) {
        if let Ok(mut guard) = self.backends.write() {
            guard.insert((backend.kind(), backend.driver_id().to_string()), backend);
        }
    }

    /// Remove the backend registered under `(kind, driver_id)`, answering
    /// whether there was one. A call already holding it finishes; the next
    /// lookup finds nothing.
    pub fn unregister(&self, kind: ImplementationKind, driver_id: &str) -> bool {
        self.backends
            .write()
            .map(|mut guard| guard.remove(&(kind, driver_id.to_string())).is_some())
            .unwrap_or(false)
    }

    /// Look up a registered [`ProviderBackend`].
    pub fn get(
        &self,
        kind: ImplementationKind,
        driver_id: &str,
    ) -> Option<Arc<dyn ProviderBackend>> {
        self.backends
            .read()
            .ok()?
            .get(&(kind, driver_id.to_string()))
            .cloned()
    }

    /// List all registered `(ImplementationKind, driver_id)` pairs.
    pub fn registered_drivers(&self) -> Vec<(ImplementationKind, String)> {
        self.backends
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Execute one logical invocation attempt through the `V2-10.1` pipeline:
    /// 1. Check runtime security lease and monotonic deny-set (`AdmissionPolicy::check_runtime_lease`).
    /// 2. Resolve the registered `ProviderBackend` for `(kind, driver_id)`.
    /// 3. Allocate `AttemptContext` (verifying cancellation, deadline, and attempt budgets).
    /// 4. Dispatch `backend.invoke(...)` and merge observed `EffectStatus` into the
    ///    invocation's monotonic ledger.
    pub async fn dispatch_attempt(
        &self,
        admission: &dyn AdmissionPolicy,
        invocation: &Invocation,
        kind: ImplementationKind,
        driver_id: &str,
        input: Value,
    ) -> ConnectorResult {
        let prior_effect = invocation.effect_status();
        let conn = invocation.connection();

        if let Err(err) = admission.check_runtime_lease(
            invocation.context().generation_id(),
            &conn.provider.provider_id,
            &conn.connection_id,
            prior_effect,
        ) {
            return ConnectorResult::Error { error: err };
        }

        let Some(backend) = self.get(kind, driver_id) else {
            return ConnectorResult::Error {
                error: ConnectorError::forbidden(
                    codes::CONNECTION_FORBIDDEN,
                    "dispatch",
                    format!("No provider backend registered for ({kind}, '{driver_id}')"),
                    prior_effect,
                ),
            };
        };

        let attempt = match invocation.next_attempt() {
            Ok(a) => a,
            Err(err) => return ConnectorResult::Error { error: err },
        };

        match backend
            .invoke(&attempt, invocation.operation(), input)
            .await
        {
            Ok(output) => {
                invocation.record_effect(EffectStatus::Completed);
                ConnectorResult::Success {
                    data: output.data,
                    metadata: output.metadata,
                }
            }
            Err(mut err) => {
                // Never allow a provider error to downgrade prior effect uncertainty (`V2-10.1`).
                let merged = invocation.record_effect(err.effect_status);
                err.effect_status = merged;
                ConnectorResult::Error { error: err }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::admission::CommunityAdmissionPolicy;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{ConnectionReference, ProviderReference};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    struct SampleVendorRustProvider;

    impl ProviderBackend for SampleVendorRustProvider {
        fn kind(&self) -> ImplementationKind {
            ImplementationKind::Rust
        }

        fn driver_id(&self) -> &str {
            "acme.custom-db"
        }

        fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
            if config.get("endpoint").and_then(Value::as_str).is_some() {
                Ok(())
            } else {
                Err(ConnectorError::invalid_input("endpoint is required", None))
            }
        }

        fn invoke<'a>(
            &'a self,
            ctx: &'a AttemptContext,
            operation: &'a str,
            input: Value,
        ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
            Box::pin(async move {
                Ok(ConnectorOutput::new(json!({
                    "driver": self.driver_id(),
                    "operation": operation,
                    "attempt": ctx.attempt_number(),
                    "echo": input,
                })))
            })
        }
    }

    #[tokio::test]
    async fn external_provider_registers_and_dispatches_without_core_enum_or_handler() {
        let registry = ProviderRegistry::new();
        registry.register(Arc::new(SampleVendorRustProvider));
        let policy = CommunityAdmissionPolicy::new();

        let provider = ProviderReference {
            provider_id: "acme.custom-db".to_string(),
            version: "1.0.0".to_string(),
            package_digest:
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
        };
        let conn = ConnectionReference {
            connection_id: "acme-primary".to_string(),
            revision: 1,
            provider,
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: conn.provider.package_digest.clone(),
            connection_id: conn.connection_id.clone(),
            revision: 1,
            secret_hash: "sha256:bbbb".to_string(),
        };

        let mut allowed_operations = BTreeMap::new();
        allowed_operations.insert(
            "acme-primary".to_string(),
            BTreeSet::from(["query".to_string()]),
        );
        let run = AdmittedRun::admit(
            "run-vendor",
            1,
            Instant::now() + Duration::from_secs(10),
            BudgetLimits::default(),
            AuthorityGrant {
                allowed_operations,
                declared_dependencies: BTreeMap::new(),
                allowed_channels: BTreeSet::new(),
            },
        );
        let inv = run
            .context()
            .start_invocation(conn, eff_id, "query", None, 2)
            .expect("invocation starts");

        let res = registry
            .dispatch_attempt(
                &policy,
                &inv,
                ImplementationKind::Rust,
                "acme.custom-db",
                json!({"sql": "SELECT 1"}),
            )
            .await;

        assert_eq!(
            res,
            ConnectorResult::Success {
                data: json!({
                    "driver": "acme.custom-db",
                    "operation": "query",
                    "attempt": 1,
                    "echo": {"sql": "SELECT 1"}
                }),
                metadata: BTreeMap::new(),
            }
        );
        assert_eq!(inv.effect_status(), EffectStatus::Completed);
    }
}
