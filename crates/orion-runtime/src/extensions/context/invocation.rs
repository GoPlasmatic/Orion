use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orion_extension_api::{
    AttemptContext, AttemptScope, ConnectionReference, ConnectionResolver, ConnectorError,
    ConnectorOutput, DependencyCaller, EffectStatus, EffectiveConnectionIdentity, SecretValue,
    codes,
};

use super::{AuthorityGrant, BudgetLimits, ChargeVector, RunContext, next_unique_id};

/// Logical connector invocation spanning retries and nested dependency calls.
#[derive(Debug, Clone)]
pub struct Invocation {
    // `pub(super)`: `RunContext` builds one from its own file, and nothing
    // outside `context` reads them (M-09).
    pub(super) invocation_id: u64,
    pub(super) parent_invocation_id: Option<u64>,
    pub(super) context: RunContext,
    pub(super) connection: ConnectionReference,
    pub(super) effective_identity: EffectiveConnectionIdentity,
    pub(super) connection_row:
        Option<Arc<crate::extensions::deployment_store::ConnectionRevisionRow>>,
    pub(super) operation: String,
    pub(super) idempotency_key: Option<String>,
    pub(super) max_attempts: u32,
    pub(super) attempt_counter: Arc<AtomicU64>,
    pub(super) effect_ledger: Arc<Mutex<EffectStatus>>,
    /// The registry a dependency call dispatches on, once the invocation is
    /// dispatched ([`Self::with_registry`]): what makes it the
    /// [`DependencyCaller`] its provider's attempts see.
    pub(super) providers: Option<crate::extensions::provider_backend::ProviderRegistry>,
}

impl Invocation {
    /// A copy bound to `providers`, its deployment store and the governed
    /// connection row: what [`ProviderRegistry::dispatch_attempt`] hands a
    /// provider, so the attempt can resolve its configuration and call its
    /// declared dependencies.
    ///
    /// [`ProviderRegistry::dispatch_attempt`]: crate::extensions::provider_backend::ProviderRegistry::dispatch_attempt
    pub fn with_registry(
        &self,
        providers: &crate::extensions::provider_backend::ProviderRegistry,
    ) -> Self {
        let mut bound = self.with_deployments(providers.deployment_store());
        bound.providers = Some(providers.clone());
        bound
    }

    /// Return a copy of this invocation bound to `deployments` if not already
    /// bound to a governed connection row.
    pub fn with_deployments(
        &self,
        deployments: &crate::extensions::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        let mut cloned = self.clone();
        if cloned.context.deployments.is_none() {
            cloned.context.deployments = Some(deployments.clone());
        }
        if cloned.connection_row.is_none() {
            cloned.connection_row = crate::extensions::foundation::governed_connection(
                Some(deployments),
                &cloned.effective_identity,
            );
        }
        cloned
    }

    pub fn invocation_id(&self) -> u64 {
        self.invocation_id
    }

    pub fn parent_invocation_id(&self) -> Option<u64> {
        self.parent_invocation_id
    }

    pub fn context(&self) -> &RunContext {
        &self.context
    }

    pub fn connection(&self) -> &ConnectionReference {
        &self.connection
    }

    pub fn effective_identity(&self) -> &EffectiveConnectionIdentity {
        &self.effective_identity
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    pub fn effect_status(&self) -> EffectStatus {
        *self.effect_ledger.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Merge newly observed effect evidence into the invocation's monotonic ledger.
    pub fn record_effect(&self, observed: EffectStatus) -> EffectStatus {
        let mut guard = self.effect_ledger.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.merge(observed);
        *guard
    }

    /// Allocate the next attempt context, enforcing both the per-invocation attempt
    /// cap and the shared root/ancestor attempt budget.
    pub fn next_attempt(&self) -> Result<AttemptContext, ConnectorError> {
        let prior_effect = self.effect_status();
        self.context.check_live(prior_effect)?;

        let current = self.attempt_counter.load(Ordering::SeqCst);
        if current >= u64::from(self.max_attempts) {
            return Err(ConnectorError::budget_exhausted(
                format!(
                    "Invocation reached local attempt ceiling ({})",
                    self.max_attempts
                ),
                prior_effect,
            ));
        }

        // Charge 1 attempt against the shared hierarchical budget coordinator.
        let reservation = self
            .context
            .budget
            .reserve(ChargeVector::attempt_only(), prior_effect)?;
        let _lease = reservation.commit_started();

        let attempt_number = self.attempt_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(AttemptContext::new(
            Arc::new(self.clone()),
            u32::try_from(attempt_number).unwrap_or(u32::MAX),
        ))
    }

    /// Spawn a child dependency invocation declared in the workflow-group manifest.
    pub fn derive_dependency(
        &self,
        declared_alias: &str,
        effective_identity: EffectiveConnectionIdentity,
        operation: impl Into<String>,
        requested_deadline: Instant,
        requested_limits: BudgetLimits,
        max_invocation_attempts: u32,
    ) -> Result<Invocation, ConnectorError> {
        let prior_effect = self.effect_status();
        self.context.check_live(prior_effect)?;
        let operation = operation.into();

        let Some((conn_ref, allowed_ops)) = self
            .context
            .authority
            .declared_dependencies
            .get(declared_alias)
            .cloned()
        else {
            return Err(ConnectorError::forbidden(
                codes::DEPENDENCY_UNDECLARED,
                "dispatch",
                format!("Dependency alias '{declared_alias}' is not declared in package manifest"),
                prior_effect,
            ));
        };

        if !allowed_ops.contains(&operation) {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!(
                    "Operation '{operation}' is not granted on dependency alias '{declared_alias}'"
                ),
                prior_effect,
            ));
        }

        let mut child_ops = BTreeMap::new();
        child_ops.insert(conn_ref.connection_id.to_string(), allowed_ops);
        let child_authority = AuthorityGrant {
            allowed_operations: child_ops,
            declared_dependencies: BTreeMap::new(),
            allowed_channels: BTreeSet::new(),
        };

        // Construct child context with reduced authority and clamped deadline.
        let next_depth = self.context.depth.saturating_add(1);
        let max_depth = self
            .context
            .budget
            .limits()
            .max_depth
            .min(requested_limits.max_depth);
        if next_depth > max_depth {
            return Err(ConnectorError::budget_exhausted(
                format!("Nested dependency depth {next_depth} exceeds maximum {max_depth}"),
                prior_effect,
            ));
        }

        let child_ctx = RunContext {
            run_id: self.context.run_id.clone(),
            generation_id: self.context.generation_id,
            deadline: self.context.deadline.min(requested_deadline),
            depth: next_depth,
            cancellation: self.context.cancellation.child(),
            accountant: self.context.accountant.clone(),
            deployments: self.context.deployments.clone(),
            budget: self.context.budget.derive_child(requested_limits),
            authority: child_authority,
        };
        let connection_row = crate::extensions::foundation::governed_connection(
            child_ctx.deployments.as_ref(),
            &effective_identity,
        );

        let child_invocation_id = next_unique_id();
        Ok(Invocation {
            invocation_id: child_invocation_id,
            parent_invocation_id: Some(self.invocation_id),
            context: child_ctx,
            connection: conn_ref,
            effective_identity,
            connection_row,
            operation,
            idempotency_key: self.idempotency_key.clone(),
            max_attempts: max_invocation_attempts.max(1),
            attempt_counter: Arc::new(AtomicU64::new(0)),
            effect_ledger: self.effect_ledger.clone(),
            providers: None,
        })
    }
}

#[async_trait::async_trait]
impl ConnectionResolver for Invocation {
    fn public_config(&self) -> Option<&serde_json::Value> {
        self.connection_row.as_ref().map(|r| &r.public_config)
    }

    async fn resolve_secret(&self, path: &str) -> Result<Option<SecretValue>, ConnectorError> {
        let Some(conn) = self.connection_row.as_deref() else {
            return Ok(None);
        };
        crate::extensions::governed_connection::resolve_secret(
            conn,
            path,
            &self.effective_identity,
            self.connection.provider.provider_id.as_str(),
        )
        .await
    }

    async fn resolve_merged_config(&self) -> Result<Option<serde_json::Value>, ConnectorError> {
        let Some(conn) = self.connection_row.as_deref() else {
            return Ok(None);
        };
        crate::extensions::governed_connection::merged_config(
            conn,
            &self.effective_identity,
            self.connection.provider.provider_id.as_str(),
        )
        .await
        .map(Some)
    }
}

/// The node's invocation state is what a provider's [`AttemptContext`] views.
impl AttemptScope for Invocation {
    fn run_id(&self) -> &str {
        self.context.run_id()
    }

    fn generation_id(&self) -> u64 {
        self.context.generation_id()
    }

    fn deadline(&self) -> Instant {
        self.context.deadline()
    }

    fn depth(&self) -> u32 {
        self.context.depth()
    }

    fn is_cancelled(&self) -> bool {
        self.context.cancellation().is_cancelled()
    }

    fn invocation_id(&self) -> u64 {
        self.invocation_id
    }

    fn parent_invocation_id(&self) -> Option<u64> {
        self.parent_invocation_id
    }

    fn connection(&self) -> &ConnectionReference {
        &self.connection
    }

    fn effective_identity(&self) -> &EffectiveConnectionIdentity {
        &self.effective_identity
    }

    fn operation(&self) -> &str {
        &self.operation
    }

    fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    fn effect_status(&self) -> EffectStatus {
        Invocation::effect_status(self)
    }

    fn record_effect(&self, observed: EffectStatus) -> EffectStatus {
        Invocation::record_effect(self, observed)
    }

    fn connection_resolver(&self) -> Option<&dyn ConnectionResolver> {
        self.connection_row.is_some().then_some(self)
    }

    fn dependency_caller(&self) -> Option<&dyn DependencyCaller> {
        self.providers.is_some().then_some(self)
    }
}

/// A dispatched invocation calls its provider's declared dependencies: the
/// alias resolves through the run's authority, keyed by this connection, to
/// the revision the deployment pins.
#[async_trait::async_trait]
impl DependencyCaller for Invocation {
    async fn call(
        &self,
        alias: &str,
        operation: &str,
        input: serde_json::Value,
    ) -> Result<ConnectorOutput, ConnectorError> {
        let Some(providers) = &self.providers else {
            return Err(ConnectorError::forbidden(
                codes::DEPENDENCY_UNDECLARED,
                "dispatch",
                format!("no dependency '{alias}' is bound for this call"),
                self.effect_status(),
            ));
        };
        crate::extensions::dependency::dispatch_dependency(providers, self, alias, operation, input)
            .await
    }

    fn bound(&self, alias: &str) -> Option<ConnectionReference> {
        let key = crate::extensions::workflow_provider::dependency_key(
            &self.connection.connection_id,
            alias,
        );
        self.context
            .authority
            .declared_dependencies
            .get(&key)
            .map(|(bound, _)| bound.clone())
    }
}
