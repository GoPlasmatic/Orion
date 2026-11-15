use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use orion_extension_api::{ConnectorError, EffectStatus};

use super::next_unique_id;

/// Multi-dimensional work charge vector.
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
/// under one critical section.
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
    /// configured limits.
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
    /// ancestor up to the root before modifying any counter.
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

/// An unstarted budget reservation.
///
/// If dropped or explicitly aborted via [`Self::abort_before_start`] before work
/// begins, both cumulative and occupancy counters are rolled back.
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

/// Occupancy lease committed to a started job.
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
