//! Owned run context and atomic budget coordinator (`V2-05.1`).
//!
//! Implements the host-owned run context (`V2-05.1`):
//! - [`AdmittedRun`] and [`RunContext`]: generation-pinned execution scope with
//!   monotonic deadline clamping (`min(parent, child)`), cooperative cancellation
//!   tree, and authority intersection (`CTX-01`, `CTX-06`).
//! - [`BudgetScope`], [`ChargeVector`], [`Reservation`], and [`WorkLease`]:
//!   atomic multi-scope reservation coordinator under a single root critical
//!   section (`CTX-02`, `CTX-03`, `CTX-04`, `CTX-10`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orion_extension_api::{
    AttemptContext, AttemptScope, ConnectionReference, ConnectorError, EffectStatus,
    EffectiveConnectionIdentity, codes,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_unique_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Multi-dimensional work charge vector (`V2-05.1`).
///
/// Distinguishes cumulative debits (`attempts`, `tasks`, `network_sends`, `bytes`)
/// from active occupancy (`active_jobs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChargeVector {
    pub attempts: u64,
    pub tasks: u64,
    pub network_sends: u64,
    pub bytes: u64,
    pub active_jobs: u64,
}

impl ChargeVector {
    pub const fn attempt_only() -> Self {
        Self {
            attempts: 1,
            tasks: 0,
            network_sends: 0,
            bytes: 0,
            active_jobs: 0,
        }
    }

    pub const fn network_job(bytes: u64) -> Self {
        Self {
            attempts: 0,
            tasks: 0,
            network_sends: 1,
            bytes,
            active_jobs: 1,
        }
    }
}

pub use crate::config::ConnectorBudgetLimits as BudgetLimits;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetCounters {
    pub spent_attempts: u64,
    pub spent_tasks: u64,
    pub spent_network_sends: u64,
    pub spent_bytes: u64,
    pub active_jobs: u64,
}

impl BudgetCounters {
    fn can_accommodate(&self, charge: &ChargeVector, limits: &BudgetLimits) -> bool {
        self.spent_attempts
            .checked_add(charge.attempts)
            .is_some_and(|v| v <= limits.max_attempts)
            && self
                .spent_tasks
                .checked_add(charge.tasks)
                .is_some_and(|v| v <= limits.max_tasks)
            && self
                .spent_network_sends
                .checked_add(charge.network_sends)
                .is_some_and(|v| v <= limits.max_network_sends)
            && self
                .spent_bytes
                .checked_add(charge.bytes)
                .is_some_and(|v| v <= limits.max_bytes)
            && self
                .active_jobs
                .checked_add(charge.active_jobs)
                .is_some_and(|v| v <= limits.max_active_jobs)
    }

    fn apply_reserve(&mut self, charge: &ChargeVector) {
        self.spent_attempts = self.spent_attempts.saturating_add(charge.attempts);
        self.spent_tasks = self.spent_tasks.saturating_add(charge.tasks);
        self.spent_network_sends = self
            .spent_network_sends
            .saturating_add(charge.network_sends);
        self.spent_bytes = self.spent_bytes.saturating_add(charge.bytes);
        self.active_jobs = self.active_jobs.saturating_add(charge.active_jobs);
    }

    fn rollback_unstarted(&mut self, charge: &ChargeVector) {
        self.spent_attempts = self.spent_attempts.saturating_sub(charge.attempts);
        self.spent_tasks = self.spent_tasks.saturating_sub(charge.tasks);
        self.spent_network_sends = self
            .spent_network_sends
            .saturating_sub(charge.network_sends);
        self.spent_bytes = self.spent_bytes.saturating_sub(charge.bytes);
        self.active_jobs = self.active_jobs.saturating_sub(charge.active_jobs);
    }

    fn release_occupancy(&mut self, active_jobs: u64) {
        self.active_jobs = self.active_jobs.saturating_sub(active_jobs);
    }
}

#[derive(Debug)]
struct ScopeNode {
    parent: Option<u64>,
    limits: BudgetLimits,
    counters: BudgetCounters,
}

/// Root-owned reservation coordinator that serializes multi-scope reservations
/// under one critical section (`CTX-02`, `CTX-03`, `CTX-10`).
#[derive(Debug)]
struct RootBudgetCoordinator {
    scopes: Mutex<BTreeMap<u64, ScopeNode>>,
}

/// Handle to a root or child budget scope sharing the root coordinator.
#[derive(Debug, Clone)]
pub struct BudgetScope {
    scope_id: u64,
    coordinator: Arc<RootBudgetCoordinator>,
}

impl BudgetScope {
    pub fn new_root(limits: BudgetLimits) -> Self {
        let scope_id = next_unique_id();
        let mut map = BTreeMap::new();
        map.insert(
            scope_id,
            ScopeNode {
                parent: None,
                limits,
                counters: BudgetCounters::default(),
            },
        );
        Self {
            scope_id,
            coordinator: Arc::new(RootBudgetCoordinator {
                scopes: Mutex::new(map),
            }),
        }
    }

    /// Derive a child budget scope whose local ceilings cannot exceed its parent's
    /// configured limits (`CTX-06`, `CTX-10`).
    pub fn derive_child(&self, requested: BudgetLimits) -> Self {
        let child_id = next_unique_id();
        let mut guard = self
            .coordinator
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parent_limits = guard
            .get(&self.scope_id)
            .map(|n| n.limits)
            .unwrap_or_default();
        let clamped = BudgetLimits {
            max_attempts: requested.max_attempts.min(parent_limits.max_attempts),
            max_tasks: requested.max_tasks.min(parent_limits.max_tasks),
            max_network_sends: requested
                .max_network_sends
                .min(parent_limits.max_network_sends),
            max_bytes: requested.max_bytes.min(parent_limits.max_bytes),
            max_active_jobs: requested.max_active_jobs.min(parent_limits.max_active_jobs),
            max_depth: requested.max_depth.min(parent_limits.max_depth),
        };
        guard.insert(
            child_id,
            ScopeNode {
                parent: Some(self.scope_id),
                limits: clamped,
                counters: BudgetCounters::default(),
            },
        );
        Self {
            scope_id: child_id,
            coordinator: self.coordinator.clone(),
        }
    }

    /// Atomically check the full `charge` vector against this scope and every
    /// ancestor up to the root before modifying any counter (`CTX-02`, `CTX-03`).
    pub fn reserve(
        &self,
        charge: ChargeVector,
        prior_effect: EffectStatus,
    ) -> Result<Reservation, ConnectorError> {
        let mut guard = self
            .coordinator
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // 1. Collect ancestor chain.
        let mut chain = Vec::new();
        let mut cursor = Some(self.scope_id);
        while let Some(id) = cursor {
            let Some(node) = guard.get(&id) else {
                break;
            };
            if !node.counters.can_accommodate(&charge, &node.limits) {
                return Err(ConnectorError::budget_exhausted(
                    "Connector work or occupancy budget exhausted",
                    prior_effect,
                ));
            }
            chain.push(id);
            cursor = node.parent;
        }

        // 2. All ancestors can accommodate the charge; apply atomically.
        for id in &chain {
            if let Some(node) = guard.get_mut(id) {
                node.counters.apply_reserve(&charge);
            }
        }

        Ok(Reservation {
            chain,
            charge,
            coordinator: self.coordinator.clone(),
            committed: false,
        })
    }

    pub fn snapshot(&self) -> BudgetCounters {
        let guard = self
            .coordinator
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .get(&self.scope_id)
            .map(|n| n.counters)
            .unwrap_or_default()
    }

    pub fn limits(&self) -> BudgetLimits {
        let guard = self
            .coordinator
            .scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .get(&self.scope_id)
            .map(|n| n.limits)
            .unwrap_or_default()
    }
}

/// An unstarted budget reservation (`V2-05.1`).
///
/// If dropped or explicitly aborted via [`Self::abort_before_start`] before work
/// begins, both cumulative and occupancy counters are rolled back (`CTX-04`).
/// Calling [`Self::commit_started`] makes cumulative debits permanent and returns
/// a [`WorkLease`] that holds occupancy until actual completion.
#[derive(Debug)]
pub struct Reservation {
    chain: Vec<u64>,
    charge: ChargeVector,
    coordinator: Arc<RootBudgetCoordinator>,
    committed: bool,
}

impl Reservation {
    pub fn commit_started(mut self) -> WorkLease {
        self.committed = true;
        WorkLease {
            chain: std::mem::take(&mut self.chain),
            active_jobs: self.charge.active_jobs,
            coordinator: self.coordinator.clone(),
            released: false,
        }
    }

    pub fn abort_before_start(self) {
        drop(self);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed {
            let mut guard = self
                .coordinator
                .scopes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for id in &self.chain {
                if let Some(node) = guard.get_mut(id) {
                    node.counters.rollback_unstarted(&self.charge);
                }
            }
        }
    }
}

/// Occupancy lease committed to a started job (`V2-05.1`).
///
/// Retains `active_jobs` occupancy until the job finishes and drops this lease,
/// while cumulative debits (`attempts`, `tasks`, `network_sends`, `bytes`) remain
/// permanently spent.
#[derive(Debug)]
pub struct WorkLease {
    chain: Vec<u64>,
    active_jobs: u64,
    coordinator: Arc<RootBudgetCoordinator>,
    released: bool,
}

impl WorkLease {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if !self.released && self.active_jobs > 0 {
            self.released = true;
            let mut guard = self
                .coordinator
                .scopes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for id in &self.chain {
                if let Some(node) = guard.get_mut(id) {
                    node.counters.release_occupancy(self.active_jobs);
                }
            }
        } else {
            self.released = true;
        }
    }
}

impl Drop for WorkLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Authority projection carried by a `RunContext` (`V2-05.1`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorityGrant {
    /// Allowed `(connection_id, operation)` pairs in this context.
    pub allowed_operations: BTreeMap<String, BTreeSet<String>>,
    /// Declared dependency aliases mapped to `(connection_id, allowed_operations)`.
    pub declared_dependencies: BTreeMap<String, (ConnectionReference, BTreeSet<String>)>,
    /// Allowed channel targets for nested `channel_call` or workflow sub-channels.
    pub allowed_channels: BTreeSet<String>,
}

impl AuthorityGrant {
    /// Whether `operation` on `connection_id` is granted.
    pub fn allows(&self, connection_id: &str, operation: &str) -> bool {
        self.allowed_operations
            .get(connection_id)
            .is_some_and(|ops| ops.contains(operation))
    }

    /// Add every operation `other` grants.
    pub fn extend(&mut self, other: &AuthorityGrant) {
        for (conn, ops) in &other.allowed_operations {
            self.allowed_operations
                .entry(conn.clone())
                .or_default()
                .extend(ops.iter().cloned());
        }
        for (alias, dep) in &other.declared_dependencies {
            self.declared_dependencies
                .entry(alias.clone())
                .or_insert_with(|| dep.clone());
        }
        self.allowed_channels
            .extend(other.allowed_channels.iter().cloned());
    }

    pub fn intersect(&self, requested: &AuthorityGrant) -> Self {
        let mut allowed_operations = BTreeMap::new();
        for (conn_id, req_ops) in &requested.allowed_operations {
            if let Some(parent_ops) = self.allowed_operations.get(conn_id) {
                let intersected: BTreeSet<String> =
                    parent_ops.intersection(req_ops).cloned().collect();
                if !intersected.is_empty() {
                    allowed_operations.insert(conn_id.clone(), intersected);
                }
            }
        }
        let mut declared_dependencies = BTreeMap::new();
        for (alias, (conn_ref, req_ops)) in &requested.declared_dependencies {
            if let Some((parent_ref, parent_ops)) = self.declared_dependencies.get(alias)
                && parent_ref == conn_ref
            {
                let intersected: BTreeSet<String> =
                    parent_ops.intersection(req_ops).cloned().collect();
                if !intersected.is_empty() {
                    declared_dependencies.insert(alias.clone(), (conn_ref.clone(), intersected));
                }
            }
        }
        let allowed_channels = self
            .allowed_channels
            .intersection(&requested.allowed_channels)
            .cloned()
            .collect();

        Self {
            allowed_operations,
            declared_dependencies,
            allowed_channels,
        }
    }
}

/// Root admitted execution handle (`V2-05.1`).
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

/// Generation-pinned execution context for root or child runs (`V2-05.1`).
#[derive(Clone)]
pub struct RunContext {
    run_id: String,
    generation_id: u64,
    deadline: Instant,
    depth: u32,
    cancellation: dataflow_rs::run::CancelToken,
    accountant: Option<Arc<crate::engine::SharedRunAccountant>>,
    budget: BudgetScope,
    authority: AuthorityGrant,
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

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Pinned generation ID; preserved across all child calls (`CTX-01`).
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

    /// Derive a child context for a nested channel or sub-workflow call (`CTX-01`, `CTX-06`).
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
            budget: self.budget.derive_child(requested_limits),
            authority: self.authority.intersect(requested_authority),
        })
    }

    /// Start a logical connector invocation after verifying authority, cancellation,
    /// and deadline (`V2-05.1`).
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

        let invocation_id = next_unique_id();
        Ok(Invocation {
            invocation_id,
            parent_invocation_id: None,
            context: self.clone(),
            connection,
            effective_identity,
            operation,
            idempotency_key,
            max_attempts: max_invocation_attempts.max(1),
            attempt_counter: Arc::new(AtomicU64::new(0)),
            effect_ledger: Arc::new(Mutex::new(EffectStatus::NotStarted)),
        })
    }

    /// Start a host-authorized source lifecycle invocation (`orion.internal.source.*`) (`V2-15.1`).
    ///
    /// Restricted to `pub(crate)` so only the host [`super::source_runner::SourceChannelRunner`]
    /// can mint privileged source facet calls while application workflows are blocked by
    /// [`Self::start_invocation`].
    pub(crate) fn start_privileged_source_invocation(
        &self,
        connection: ConnectionReference,
        effective_identity: EffectiveConnectionIdentity,
        facet_step: &str,
        max_invocation_attempts: u32,
    ) -> Result<Invocation, ConnectorError> {
        self.check_live(EffectStatus::NotStarted)?;
        let operation = format!("orion.internal.source.{facet_step}");
        let invocation_id = next_unique_id();
        Ok(Invocation {
            invocation_id,
            parent_invocation_id: None,
            context: self.clone(),
            connection,
            effective_identity,
            operation,
            idempotency_key: None,
            max_attempts: max_invocation_attempts.max(1),
            attempt_counter: Arc::new(AtomicU64::new(0)),
            effect_ledger: Arc::new(Mutex::new(EffectStatus::NotStarted)),
        })
    }
}

/// Logical connector invocation spanning retries and nested dependency calls (`V2-05.1`).
#[derive(Debug, Clone)]
pub struct Invocation {
    invocation_id: u64,
    parent_invocation_id: Option<u64>,
    context: RunContext,
    connection: ConnectionReference,
    effective_identity: EffectiveConnectionIdentity,
    operation: String,
    idempotency_key: Option<String>,
    max_attempts: u32,
    attempt_counter: Arc<AtomicU64>,
    effect_ledger: Arc<Mutex<EffectStatus>>,
}

impl Invocation {
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

    /// Merge newly observed effect evidence into the invocation's monotonic ledger (`V2-10.1`).
    pub fn record_effect(&self, observed: EffectStatus) -> EffectStatus {
        let mut guard = self.effect_ledger.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.merge(observed);
        *guard
    }

    /// Allocate the next attempt context, enforcing both the per-invocation attempt
    /// cap and the shared root/ancestor attempt budget (`CTX-10`).
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

    /// Spawn a child dependency invocation declared in the workflow-group manifest (`V2-05.1`).
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
            budget: self.context.budget.derive_child(requested_limits),
            authority: child_authority,
        };

        let child_invocation_id = next_unique_id();
        Ok(Invocation {
            invocation_id: child_invocation_id,
            parent_invocation_id: Some(self.invocation_id),
            context: child_ctx,
            connection: conn_ref,
            effective_identity,
            operation,
            idempotency_key: self.idempotency_key.clone(),
            max_attempts: max_invocation_attempts.max(1),
            attempt_counter: Arc::new(AtomicU64::new(0)),
            effect_ledger: self.effect_ledger.clone(),
        })
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
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
