//! Owned run context and atomic budget coordinator.
//!
//! Implements the host-owned run context:
//! - [`AdmittedRun`] and [`RunContext`]: generation-pinned execution scope with
//!   monotonic deadline clamping (`min(parent, child)`), cooperative cancellation
//!   tree, and authority intersection.
//! - [`BudgetScope`], [`ChargeVector`], [`Reservation`], and [`WorkLease`]:
//!   atomic multi-scope reservation coordinator under a single root critical
//!   section.
//!
//! The four concerns live in their own files; this module is the one surface
//! the rest of the runtime names them by (M-09).

use std::sync::atomic::{AtomicU64, Ordering};

mod authority;
mod budget;
mod invocation;
mod run;

pub use authority::AuthorityGrant;
pub use budget::BudgetLimits;
pub use budget::{BudgetCounters, BudgetScope, ChargeVector, Reservation, WorkLease};
pub use invocation::Invocation;
pub use run::{AdmittedRun, RunContext};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_unique_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests;
