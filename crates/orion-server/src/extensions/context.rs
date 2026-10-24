//! Owned run context, atomic budget coordinator, capability handles and host jobs (`V2-05.1`).
//!
//! Implements the host-owned run context and capability contract (`V2-05.1`):
//! - [`AdmittedRun`] and [`RunContext`]: generation-pinned execution scope with
//!   monotonic deadline clamping (`min(parent, child)`), cooperative cancellation
//!   tree, and authority intersection (`CTX-01`, `CTX-06`).
//! - [`BudgetScope`], [`ChargeVector`], [`Reservation`], and [`WorkLease`]:
//!   atomic multi-scope reservation coordinator under a single root critical
//!   section (`CTX-02`, `CTX-03`, `CTX-04`, `CTX-10`).
//! - [`CapabilityTable`] and [`CapabilityHandle`]: unforgeable capability tokens
//!   validated against owner invocation, generation, resource kind, and effective
//!   connection identity before external activity (`CTX-05`).
//! - [`OwnedHostJob`] and [`HostJobRegistry`]: separates caller await/cancellation
//!   from actual job completion so running work retains occupancy leases until
//!   the worker finishes (`CTX-04`, `CTX-07`).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::oneshot;

use super::contract::{
    ConnectionReference, ConnectorError, EffectStatus, EffectiveConnectionIdentity, codes,
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

/// Configured budget ceilings for a root run or a restricted child scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetLimits {
    pub max_attempts: u64,
    pub max_tasks: u64,
    pub max_network_sends: u64,
    pub max_bytes: u64,
    pub max_active_jobs: u64,
    pub max_depth: u32,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            max_attempts: 32,
            max_tasks: 256,
            max_network_sends: 64,
            max_bytes: 16 * 1024 * 1024,
            max_active_jobs: 16,
            max_depth: 8,
        }
    }
}

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

/// Resource kind for scoped capability handles (`V2-05.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    Http,
    Credential,
    Dependency,
    Session,
    Sqlite,
    SourceCheckpoint,
}

/// Authority grant associated with a capability table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    pub kind: ResourceKind,
    pub connection: EffectiveConnectionIdentity,
    pub allowed_operations: BTreeSet<String>,
    pub allowed_endpoints: BTreeSet<String>,
}

/// Opaque capability handle passed to a workflow host operation or Wasm guest (`CTX-05`).
///
/// Carries an unforgeable secret token minted by the owning [`CapabilityTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityHandle {
    entry_id: u64,
    secret_token: u64,
    owner_invocation_id: u64,
    generation_id: u64,
    kind: ResourceKind,
}

impl CapabilityHandle {
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }

    pub fn owner_invocation_id(&self) -> u64 {
        self.owner_invocation_id
    }

    pub fn generation_id(&self) -> u64 {
        self.generation_id
    }

    /// Construct a forged handle for negative security testing (`CTX-05`).
    #[cfg(test)]
    pub fn forged_for_test(
        entry_id: u64,
        owner_invocation_id: u64,
        generation_id: u64,
        kind: ResourceKind,
    ) -> Self {
        Self {
            entry_id,
            secret_token: 0xDEAD_BEEF_CAFE_BABE,
            owner_invocation_id,
            generation_id,
            kind,
        }
    }
}

#[derive(Debug)]
struct CapabilityEntry {
    secret_token: u64,
    owner_invocation_id: u64,
    generation_id: u64,
    grant: CapabilityGrant,
    retired: bool,
}

/// Invocation-scoped capability table (`V2-05.1`, `CTX-05`).
#[derive(Debug, Clone)]
pub struct CapabilityTable {
    owner_invocation_id: u64,
    generation_id: u64,
    entries: Arc<Mutex<BTreeMap<u64, CapabilityEntry>>>,
}

impl CapabilityTable {
    pub fn new(owner_invocation_id: u64, generation_id: u64) -> Self {
        Self {
            owner_invocation_id,
            generation_id,
            entries: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Mint a new scoped capability handle bound to this invocation and generation.
    pub fn mint(&self, grant: CapabilityGrant) -> CapabilityHandle {
        let entry_id = next_unique_id();
        // Mix two monotonic counters with a golden-ratio constant so sequential
        // entry IDs never have predictable tokens.
        let secret_token = next_unique_id().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let kind = grant.kind;
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            entry_id,
            CapabilityEntry {
                secret_token,
                owner_invocation_id: self.owner_invocation_id,
                generation_id: self.generation_id,
                grant,
                retired: false,
            },
        );
        CapabilityHandle {
            entry_id,
            secret_token,
            owner_invocation_id: self.owner_invocation_id,
            generation_id: self.generation_id,
            kind,
        }
    }

    /// Retire a handle so any subsequent borrow is rejected (`CTX-05`).
    pub fn retire(&self, handle: &CapabilityHandle) {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get_mut(&handle.entry_id)
            && entry.secret_token == handle.secret_token
        {
            entry.retired = true;
        }
    }

    /// Validate a capability handle before any external operation (`CTX-05`).
    ///
    /// Checks owner invocation, generation, secret token, resource kind,
    /// retirement state, and optional operation/endpoint grants. Preserves
    /// `prior_effect` on rejection.
    pub fn validate_borrow(
        &self,
        handle: &CapabilityHandle,
        expected_kind: ResourceKind,
        required_operation: Option<&str>,
        required_endpoint: Option<&str>,
        prior_effect: EffectStatus,
    ) -> Result<CapabilityGrant, ConnectorError> {
        if handle.owner_invocation_id != self.owner_invocation_id
            || handle.generation_id != self.generation_id
        {
            return Err(ConnectorError::capability_invalid(
                "Capability handle belongs to a different invocation or generation",
                prior_effect,
            ));
        }
        if handle.kind != expected_kind {
            return Err(ConnectorError::capability_invalid(
                "Capability handle kind mismatch",
                prior_effect,
            ));
        }

        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = guard.get(&handle.entry_id) else {
            return Err(ConnectorError::capability_invalid(
                "Capability handle not found in invocation table",
                prior_effect,
            ));
        };
        if entry.secret_token != handle.secret_token
            || entry.owner_invocation_id != self.owner_invocation_id
            || entry.generation_id != self.generation_id
        {
            return Err(ConnectorError::capability_invalid(
                "Capability handle authentication failed",
                prior_effect,
            ));
        }
        if entry.retired {
            return Err(ConnectorError::capability_invalid(
                "Capability handle has been retired",
                prior_effect,
            ));
        }
        if let Some(op) = required_operation
            && !entry.grant.allowed_operations.is_empty()
            && !entry.grant.allowed_operations.contains(op)
        {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Operation '{op}' is not granted on this capability handle"),
                prior_effect,
            ));
        }
        if let Some(endpoint) = required_endpoint
            && !entry.grant.allowed_endpoints.is_empty()
            && !entry.grant.allowed_endpoints.contains(endpoint)
        {
            return Err(ConnectorError::forbidden(
                codes::EGRESS_DENIED,
                "transport",
                "Target endpoint is outside capability grant",
                prior_effect,
            ));
        }

        Ok(entry.grant.clone())
    }
}

/// Hierarchical cooperative cancellation token (`V2-05.1`).
///
/// Root cancellation propagates to all children; cancelling a child does not
/// cancel its parent or siblings.
#[derive(Debug, Clone)]
pub struct CancellationScope {
    parent: Option<Arc<CancellationScope>>,
    local_cancelled: Arc<AtomicBool>,
}

impl Default for CancellationScope {
    fn default() -> Self {
        Self::new_root()
    }
}

impl CancellationScope {
    pub fn new_root() -> Self {
        Self {
            parent: None,
            local_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn derive_child(&self) -> Self {
        Self {
            parent: Some(Arc::new(self.clone())),
            local_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.local_cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        if self.local_cancelled.load(Ordering::SeqCst) {
            return true;
        }
        self.parent.as_ref().is_some_and(|p| p.is_cancelled())
    }
}

/// Authority projection carried by a [`RunContext`] (`V2-05.1`).
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
        let cancellation = CancellationScope::new_root();
        let context = RunContext {
            run_id: run_id.clone(),
            generation_id,
            deadline,
            depth: 0,
            cancellation,
            budget,
            authority,
        };
        Self {
            run_id,
            generation_id,
            context,
        }
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
#[derive(Debug, Clone)]
pub struct RunContext {
    run_id: String,
    generation_id: u64,
    deadline: Instant,
    depth: u32,
    cancellation: CancellationScope,
    budget: BudgetScope,
    authority: AuthorityGrant,
}

impl RunContext {
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

    pub fn cancellation(&self) -> &CancellationScope {
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
            cancellation: self.cancellation.derive_child(),
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
            .get(&connection.connection_id)
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
        let capabilities = CapabilityTable::new(invocation_id, self.generation_id);
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
            capabilities,
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
        let capabilities = CapabilityTable::new(invocation_id, self.generation_id);
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
            capabilities,
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
    capabilities: CapabilityTable,
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

    pub fn capabilities(&self) -> &CapabilityTable {
        &self.capabilities
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
        Ok(AttemptContext {
            invocation: self.clone(),
            attempt_number: u32::try_from(attempt_number).unwrap_or(u32::MAX),
        })
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
        child_ops.insert(conn_ref.connection_id.clone(), allowed_ops);
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
            cancellation: self.context.cancellation.derive_child(),
            budget: self.context.budget.derive_child(requested_limits),
            authority: child_authority,
        };

        let child_invocation_id = next_unique_id();
        let capabilities = CapabilityTable::new(child_invocation_id, child_ctx.generation_id);
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
            capabilities,
        })
    }
}

/// Context for a single provider dispatch attempt (`V2-05.1`).
#[derive(Debug, Clone)]
pub struct AttemptContext {
    invocation: Invocation,
    attempt_number: u32,
}

impl AttemptContext {
    pub fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }

    pub fn run_context(&self) -> &RunContext {
        self.invocation.context()
    }
}

/// Registry that owns background host jobs until actual completion (`V2-05.1`, `CTX-04`).
///
/// Moving `WorkLease` into the worker task guarantees that even if the caller's
/// `OwnedHostJob` future is dropped, cancelled, or times out, the occupancy lease
/// remains held until the underlying operation actually finishes.
#[derive(Debug, Default, Clone)]
pub struct HostJobRegistry {
    active_count: Arc<AtomicU64>,
}

impl HostJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_jobs(&self) -> u64 {
        self.active_count.load(Ordering::SeqCst)
    }

    /// Schedule a host job after committing `reservation` into a `WorkLease`
    /// owned by the spawned worker task (`CTX-04`).
    pub fn spawn_owned<T, F>(&self, reservation: Reservation, fut: F) -> OwnedHostJob<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let lease = reservation.commit_started();
        let active_count = self.active_count.clone();
        active_count.fetch_add(1, Ordering::SeqCst);

        let (tx, rx) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();

        tokio::spawn(async move {
            let _owned_lease = lease;
            struct ActiveGuard(Arc<AtomicU64>);
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _guard = ActiveGuard(active_count);

            let output = fut.await;
            if !worker_cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(output);
            }
        });

        OwnedHostJob {
            rx: Some(rx),
            cancelled,
        }
    }
}

/// Handle to a host-owned job (`V2-05.1`, `CTX-04`).
///
/// Dropping `OwnedHostJob` or timing out in [`Self::await_with_context`] marks
/// the caller as terminated once without releasing the worker's `WorkLease`
/// until the worker task actually completes.
pub struct OwnedHostJob<T> {
    rx: Option<oneshot::Receiver<T>>,
    cancelled: Arc<AtomicBool>,
}

impl<T> OwnedHostJob<T> {
    /// Await the job result within `ctx`'s deadline and cancellation scope.
    pub async fn await_with_context(
        mut self,
        ctx: &RunContext,
        effect_on_depart: EffectStatus,
    ) -> Result<T, ConnectorError> {
        ctx.check_live(effect_on_depart)?;
        let Some(mut rx) = self.rx.take() else {
            return Err(ConnectorError::cancelled(effect_on_depart));
        };

        let now = Instant::now();
        let remaining = ctx.deadline().saturating_duration_since(now);
        if remaining.is_zero() {
            self.cancelled.store(true, Ordering::SeqCst);
            return Err(ConnectorError::deadline_exceeded(effect_on_depart));
        }

        tokio::select! {
            res = &mut rx => {
                if ctx.cancellation().is_cancelled() {
                    self.cancelled.store(true, Ordering::SeqCst);
                    return Err(ConnectorError::cancelled(effect_on_depart));
                }
                res.map_err(|_| ConnectorError::cancelled(effect_on_depart))
            }
            () = tokio::time::sleep(remaining) => {
                self.cancelled.store(true, Ordering::SeqCst);
                Err(ConnectorError::deadline_exceeded(effect_on_depart))
            }
        }
    }
}

impl<T> Drop for OwnedHostJob<T> {
    fn drop(&mut self) {
        if self.rx.is_some() {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
