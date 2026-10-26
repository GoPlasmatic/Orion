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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::admission::RevocationLedger;
use super::context::{AttemptContext, Invocation};
use super::contract::{
    ConnectorError, ConnectorOutput, EffectStatus, EffectiveConnectionIdentity, ImplementationKind,
};
use super::deployment_store::ImmutableDeploymentStore;

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
#[async_trait]
pub trait SourceConnectorFacet: Send + Sync {
    /// Open or resume a source cursor from `checkpoint`.
    async fn open(
        &self,
        ctx: &AttemptContext,
        checkpoint: Option<Value>,
    ) -> Result<Value, ConnectorError>;

    /// Poll up to `max_items` records starting from `cursor`.
    async fn poll(
        &self,
        ctx: &AttemptContext,
        cursor: &Value,
        max_items: u32,
    ) -> Result<SourcePollBatch, ConnectorError>;

    /// Settle a delivered item (`Ack`, `NackRetryable`, or `DeadLetter`).
    async fn settle(
        &self,
        ctx: &AttemptContext,
        delivery_id: &str,
        settlement: SourceSettlement,
    ) -> Result<(), ConnectorError>;

    /// Close the source cursor and release any cursor-specific state.
    async fn close(&self, ctx: &AttemptContext, cursor: Value) -> Result<(), ConnectorError>;
}

/// Unified connector provider backend trait (`V2-11.1`, `V2-10.1`).
///
/// External Community and Enterprise providers register implementations of this
/// trait without modifying core enums or built-in handler tables. Implement it
/// with `#[async_trait::async_trait]`.
#[async_trait]
pub trait ProviderBackend: Send + Sync {
    /// Implementation kind (`Rust`, `Workflow`, or `Wasm`).
    fn kind(&self) -> ImplementationKind;

    /// Driver or adapter identifier (e.g. `"orion.http"`, `"orion.postgresql"`,
    /// `"orion.workflow-host"`).
    fn driver_id(&self) -> &str;

    /// Validate resolved connection configuration against provider-specific invariants.
    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError>;

    /// Execute one attempt of `operation` with validated `input`.
    async fn invoke(
        &self,
        ctx: &AttemptContext,
        operation: &str,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError>;

    /// Probe downstream connectivity or readiness under `ctx`.
    async fn health_check(&self, ctx: &AttemptContext) -> Result<(), ConnectorError> {
        let _ = ctx;
        Ok(())
    }

    /// Dispose any provider-held resources for `identity`.
    async fn dispose(&self, identity: &EffectiveConnectionIdentity) {
        let _ = identity;
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

/// A second backend under a `(kind, driver_id)` that already has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateProvider {
    pub kind: ImplementationKind,
    pub driver_id: String,
}

impl std::fmt::Display for DuplicateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a provider backend is already registered as ({}, '{}')",
            self.kind, self.driver_id
        )
    }
}

impl std::error::Error for DuplicateProvider {}

/// Backends by kind, then by driver id: a lookup borrows the id rather than
/// building a `(kind, String)` key.
#[derive(Clone, Default)]
pub(crate) struct ProviderTable {
    rust: BTreeMap<String, Arc<dyn ProviderBackend>>,
    workflow: BTreeMap<String, Arc<dyn ProviderBackend>>,
    wasm: BTreeMap<String, Arc<dyn ProviderBackend>>,
}

impl ProviderTable {
    fn of(&self, kind: ImplementationKind) -> &BTreeMap<String, Arc<dyn ProviderBackend>> {
        match kind {
            ImplementationKind::Rust => &self.rust,
            ImplementationKind::Workflow => &self.workflow,
            ImplementationKind::Wasm => &self.wasm,
        }
    }

    fn of_mut(
        &mut self,
        kind: ImplementationKind,
    ) -> &mut BTreeMap<String, Arc<dyn ProviderBackend>> {
        match kind {
            ImplementationKind::Rust => &mut self.rust,
            ImplementationKind::Workflow => &mut self.workflow,
            ImplementationKind::Wasm => &mut self.wasm,
        }
    }

    fn get(&self, kind: ImplementationKind, driver_id: &str) -> Option<&Arc<dyn ProviderBackend>> {
        self.of(kind).get(driver_id)
    }

    fn contains(&self, kind: ImplementationKind, driver_id: &str) -> bool {
        self.of(kind).contains_key(driver_id)
    }

    /// Add `backend`, refusing a key already held.
    pub(crate) fn insert(
        &mut self,
        backend: Arc<dyn ProviderBackend>,
    ) -> Result<(), DuplicateProvider> {
        let kind = backend.kind();
        let id = backend.driver_id().to_string();
        let map = self.of_mut(kind);
        if map.contains_key(&id) {
            return Err(DuplicateProvider {
                kind,
                driver_id: id,
            });
        }
        map.insert(id, backend);
        Ok(())
    }

    fn keys(&self) -> impl Iterator<Item = (ImplementationKind, &String)> {
        self.rust
            .keys()
            .map(|k| (ImplementationKind::Rust, k))
            .chain(
                self.workflow
                    .keys()
                    .map(|k| (ImplementationKind::Workflow, k)),
            )
            .chain(self.wasm.keys().map(|k| (ImplementationKind::Wasm, k)))
    }
}

/// The node's [`ProviderBackend`]s, in two layers (`V2-11.1`):
///
/// - **Composed**: what the edition's [`super::CompositionBuilder`] registered
///   (the foundation drivers, Enterprise's providers). Frozen when the
///   composition is built: a plain map, no lock, never changed again.
/// - **Admitted**: provider packages this node admits at runtime (an uploaded
///   `workflow` package, registered under its digest by
///   `runtime::workflow_providers::sync`). The one mutable part, and
///   explicitly so: [`Self::admit`] and [`Self::withdraw`] are its only
///   writers, readers load it without a lock, and it can never shadow or
///   remove a composed backend.
///
/// A lookup reads the composed layer first. Bound at build to the node's
/// deployment store (the connection revisions a call resolves) and its
/// [`RevocationLedger`] (checked before every dispatch).
#[derive(Clone)]
pub struct ProviderRegistry {
    composed: Arc<ProviderTable>,
    admitted: Arc<AdmittedProviders>,
    deployments: ImmutableDeploymentStore,
    ledger: Arc<RevocationLedger>,
}

/// The admitted layer: readers load the table lock-free, writers serialise
/// on `write` and publish a new table.
#[derive(Default)]
struct AdmittedProviders {
    table: ArcSwap<ProviderTable>,
    write: Mutex<()>,
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
    /// A registry over the frozen `composed` table, with an empty admitted
    /// layer. Built by [`super::CompositionBuilder::build`].
    pub(crate) fn new(
        composed: ProviderTable,
        deployments: ImmutableDeploymentStore,
        ledger: Arc<RevocationLedger>,
    ) -> Self {
        Self {
            composed: Arc::new(composed),
            admitted: Arc::new(AdmittedProviders::default()),
            deployments,
            ledger,
        }
    }

    /// The deployment store connection revisions and packages are resolved
    /// from.
    pub fn deployment_store(&self) -> &ImmutableDeploymentStore {
        &self.deployments
    }

    /// The node's revocation ledger, checked before every dispatch.
    pub fn ledger(&self) -> &Arc<RevocationLedger> {
        &self.ledger
    }

    /// Admit `backend` into the runtime layer. Refused when either layer
    /// already holds its `(kind, driver_id)`: an admitted package cannot
    /// shadow a composed provider, nor replace another admitted one.
    pub fn admit(&self, backend: Arc<dyn ProviderBackend>) -> Result<(), DuplicateProvider> {
        if self.composed.contains(backend.kind(), backend.driver_id()) {
            return Err(DuplicateProvider {
                kind: backend.kind(),
                driver_id: backend.driver_id().to_string(),
            });
        }
        let _write = self
            .admitted
            .write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut next = ProviderTable::clone(&self.admitted.table.load());
        next.insert(backend)?;
        self.admitted.table.store(Arc::new(next));
        Ok(())
    }

    /// Remove the admitted backend under `(kind, driver_id)`, answering
    /// whether there was one. A composed backend is never removed. A call
    /// already holding it finishes; the next lookup finds nothing.
    pub fn withdraw(&self, kind: ImplementationKind, driver_id: &str) -> bool {
        let _write = self
            .admitted
            .write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = self.admitted.table.load();
        if !current.contains(kind, driver_id) {
            return false;
        }
        let mut next = ProviderTable::clone(&current);
        next.of_mut(kind).remove(driver_id);
        self.admitted.table.store(Arc::new(next));
        true
    }

    /// The driver ids of `kind` in the admitted layer.
    pub fn admitted_ids(&self, kind: ImplementationKind) -> BTreeSet<String> {
        self.admitted
            .table
            .load()
            .of(kind)
            .keys()
            .cloned()
            .collect()
    }

    /// Look up a registered [`ProviderBackend`]: composed first, then admitted.
    pub fn get(
        &self,
        kind: ImplementationKind,
        driver_id: &str,
    ) -> Option<Arc<dyn ProviderBackend>> {
        if let Some(backend) = self.composed.get(kind, driver_id) {
            return Some(backend.clone());
        }
        self.admitted.table.load().get(kind, driver_id).cloned()
    }

    /// Every registered `(ImplementationKind, driver_id)` pair, both layers,
    /// sorted.
    pub fn registered_drivers(&self) -> Vec<(ImplementationKind, String)> {
        let admitted = self.admitted.table.load();
        let mut out: Vec<(ImplementationKind, String)> = self
            .composed
            .keys()
            .chain(admitted.keys())
            .map(|(kind, id)| (kind, id.clone()))
            .collect();
        out.sort();
        out
    }

    /// Execute one logical invocation attempt on `backend`, resolved once by
    /// the caller with [`Self::get`], through the `V2-10.1` pipeline:
    /// 1. Check the runtime security lease and the monotonic deny-set
    ///    ([`RevocationLedger::check_runtime_lease`]).
    /// 2. Allocate `AttemptContext` (verifying cancellation, deadline, and attempt budgets).
    /// 3. Dispatch `backend.invoke(...)` and merge observed `EffectStatus` into the
    ///    invocation's monotonic ledger.
    pub async fn dispatch_attempt(
        &self,
        invocation: &Invocation,
        backend: &dyn ProviderBackend,
        input: Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        let prior_effect = invocation.effect_status();
        let conn = invocation.connection();

        self.ledger.check_runtime_lease(
            invocation.context().generation_id(),
            &conn.provider.provider_id,
            &conn.connection_id,
            prior_effect,
        )?;

        let attempt = invocation.next_attempt()?;

        match backend
            .invoke(&attempt, invocation.operation(), input)
            .await
        {
            Ok(output) => {
                invocation.record_effect(EffectStatus::Completed);
                Ok(output)
            }
            Err(mut err) => {
                // Never allow a provider error to downgrade prior effect uncertainty (`V2-10.1`).
                let merged = invocation.record_effect(err.effect_status);
                err.effect_status = merged;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{ConnectionReference, ProviderReference};
    use serde_json::json;
    use std::time::{Duration, Instant};

    struct SampleVendorRustProvider;

    #[async_trait::async_trait]
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

        async fn invoke(
            &self,
            ctx: &AttemptContext,
            operation: &str,
            input: Value,
        ) -> Result<ConnectorOutput, ConnectorError> {
            Ok(ConnectorOutput::new(json!({
                "driver": self.driver_id(),
                "operation": operation,
                "attempt": ctx.attempt_number(),
                "echo": input,
            })))
        }
    }

    #[tokio::test]
    async fn external_provider_registers_and_dispatches_without_core_enum_or_handler() {
        let comp =
            crate::extensions::CompositionBuilder::new(crate::extensions::EditionTier::Community)
                .with_provider_backend(Arc::new(SampleVendorRustProvider))
                .build_in_memory()
                .expect("composes");
        let registry = comp.providers();

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

        let backend = registry
            .get(ImplementationKind::Rust, "acme.custom-db")
            .expect("registered");
        let res = registry
            .dispatch_attempt(&inv, backend.as_ref(), json!({"sql": "SELECT 1"}))
            .await;

        assert_eq!(
            res,
            Ok(ConnectorOutput::new(json!({
                "driver": "acme.custom-db",
                "operation": "query",
                "attempt": 1,
                "echo": {"sql": "SELECT 1"}
            })))
        );
        assert_eq!(inv.effect_status(), EffectStatus::Completed);
    }
}
