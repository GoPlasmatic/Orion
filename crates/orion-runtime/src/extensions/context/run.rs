use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orion_extension_api::{
    ConnectionReference, ConnectorError, EffectStatus, EffectiveConnectionIdentity, codes,
};

use super::{AuthorityGrant, BudgetLimits, BudgetScope, Invocation, next_unique_id};

/// Root admitted execution handle.
#[derive(Debug, Clone)]
pub struct AdmittedRun {
    run_id: String,
    generation_id: u64,
    context: RunContext,
}

impl AdmittedRun {
    pub fn admit(
        run_id: impl Into<String>,
        generation_id: u64,
        deadline: Instant,
        limits: BudgetLimits,
        authority: AuthorityGrant,
    ) -> Self {
        let run_id = run_id.into();
        let budget = BudgetScope::new_root(limits);
        let cancellation = dataflow_rs::run::CancelToken::new();
        let context = RunContext {
            run_id: run_id.clone(),
            generation_id,
            deadline,
            depth: 0,
            cancellation,
            accountant: None,
            deployments: None,
            budget,
            authority,
        };
        Self {
            run_id,
            generation_id,
            context,
        }
    }

    /// Attach the same cancellation signal and engine accounting used by the run.
    pub fn with_execution(
        mut self,
        token: dataflow_rs::run::CancelToken,
        accountant: Option<Arc<crate::engine::SharedRunAccountant>>,
    ) -> Self {
        self.context.cancellation = token;
        self.context.accountant = accountant;
        self
    }

    /// Attach the deployment store so invocations started in this run resolve
    /// their governed connection configuration and secrets via
    /// [`AttemptContext::connection_resolver`].
    pub fn with_deployments(
        mut self,
        deployments: crate::extensions::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        self.context.deployments = Some(deployments);
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn generation_id(&self) -> u64 {
        self.generation_id
    }

    pub fn context(&self) -> RunContext {
        self.context.clone()
    }
}

/// Generation-pinned execution context for root or child runs.
#[derive(Clone)]
pub struct RunContext {
    // `pub(super)`: `Invocation` reads these from its own file, and nothing
    // outside `context` does (M-09).
    pub(super) run_id: String,
    pub(super) generation_id: u64,
    pub(super) deadline: Instant,
    pub(super) depth: u32,
    pub(super) cancellation: dataflow_rs::run::CancelToken,
    pub(super) accountant: Option<Arc<crate::engine::SharedRunAccountant>>,
    pub(super) deployments: Option<crate::extensions::deployment_store::ImmutableDeploymentStore>,
    pub(super) budget: BudgetScope,
    pub(super) authority: AuthorityGrant,
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("generation_id", &self.generation_id)
            .field("deadline", &self.deadline)
            .field("depth", &self.depth)
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl RunContext {
    /// Project the current engine call's narrower deadline and cancellation token.
    /// Accounting stays shared with the admitted root.
    pub fn for_execution(
        &self,
        deadline: Option<Instant>,
        token: Option<dataflow_rs::run::CancelToken>,
    ) -> Self {
        let mut child = self.clone();
        if let Some(deadline) = deadline {
            child.deadline = child.deadline.min(deadline);
        }
        if let Some(token) = token {
            child.cancellation = token;
        }
        child
    }

    pub fn accountant(&self) -> Option<&Arc<crate::engine::SharedRunAccountant>> {
        self.accountant.as_ref()
    }

    /// Attach the deployment store so invocations started in this run resolve
    /// their governed connection configuration and secrets via
    /// [`AttemptContext::connection_resolver`].
    pub fn with_deployments(
        mut self,
        deployments: crate::extensions::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        self.deployments = Some(deployments);
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Pinned generation ID; preserved across all child calls.
    pub fn generation_id(&self) -> u64 {
        self.generation_id
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn cancellation(&self) -> &dataflow_rs::run::CancelToken {
        &self.cancellation
    }

    pub fn budget(&self) -> &BudgetScope {
        &self.budget
    }

    pub fn authority(&self) -> &AuthorityGrant {
        &self.authority
    }

    pub fn check_live(&self, prior_effect: EffectStatus) -> Result<(), ConnectorError> {
        if self.cancellation.is_cancelled() {
            return Err(ConnectorError::cancelled(prior_effect));
        }
        if Instant::now() >= self.deadline {
            return Err(ConnectorError::deadline_exceeded(prior_effect));
        }
        Ok(())
    }

    /// Derive a child context for a nested channel or sub-workflow call.
    ///
    /// - Preserves `generation_id`.
    /// - Clamps deadline to `min(parent.deadline, requested_deadline)`.
    /// - Intersects authority so a child cannot gain broader permissions.
    /// - Increments branch depth without decrementing sibling depth.
    pub fn derive_child(
        &self,
        requested_deadline: Instant,
        requested_limits: BudgetLimits,
        requested_authority: &AuthorityGrant,
    ) -> Result<Self, ConnectorError> {
        self.check_live(EffectStatus::NotStarted)?;
        let next_depth = self.depth.saturating_add(1);
        let max_depth = self
            .budget
            .limits()
            .max_depth
            .min(requested_limits.max_depth);
        if next_depth > max_depth {
            return Err(ConnectorError::budget_exhausted(
                format!("Nested call depth {next_depth} exceeds maximum {max_depth}"),
                EffectStatus::NotStarted,
            ));
        }

        Ok(Self {
            run_id: self.run_id.clone(),
            generation_id: self.generation_id,
            deadline: self.deadline.min(requested_deadline),
            depth: next_depth,
            cancellation: self.cancellation.child(),
            accountant: self.accountant.clone(),
            deployments: self.deployments.clone(),
            budget: self.budget.derive_child(requested_limits),
            authority: self.authority.intersect(requested_authority),
        })
    }

    /// Start a logical connector invocation after verifying authority, cancellation,
    /// and deadline.
    pub fn start_invocation(
        &self,
        connection: ConnectionReference,
        effective_identity: EffectiveConnectionIdentity,
        operation: impl Into<String>,
        idempotency_key: Option<String>,
        max_invocation_attempts: u32,
    ) -> Result<Invocation, ConnectorError> {
        self.check_live(EffectStatus::NotStarted)?;
        let operation = operation.into();
        if operation.starts_with("orion.internal.") {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                "Reserved internal operation name cannot be invoked at application boundary",
                EffectStatus::NotStarted,
            ));
        }

        let Some(allowed_ops) = self
            .authority
            .allowed_operations
            .get(connection.connection_id.as_str())
        else {
            return Err(ConnectorError::forbidden(
                codes::CONNECTION_FORBIDDEN,
                "dispatch",
                format!(
                    "Connection '{}' is not authorized in this context",
                    connection.connection_id
                ),
                EffectStatus::NotStarted,
            ));
        };
        if !allowed_ops.contains(&operation) {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!(
                    "Operation '{operation}' on connection '{}' is not authorized",
                    connection.connection_id
                ),
                EffectStatus::NotStarted,
            ));
        }

        let connection_row = crate::extensions::foundation::governed_connection(
            self.deployments.as_ref(),
            &effective_identity,
        );
        let invocation_id = next_unique_id();
        Ok(Invocation {
            invocation_id,
            parent_invocation_id: None,
            context: self.clone(),
            connection,
            effective_identity,
            connection_row,
            operation,
            idempotency_key,
            max_attempts: max_invocation_attempts.max(1),
            attempt_counter: Arc::new(AtomicU64::new(0)),
            effect_ledger: Arc::new(Mutex::new(EffectStatus::NotStarted)),
            providers: None,
        })
    }
}
