//! What the scheduler tells `/health` about itself.
//!
//! The supervisor already reports whether the two loops are *alive*, and that
//! is not the interesting failure. A reconciler that is running but has not
//! completed a pass in an hour — because every pass errors against an
//! unreachable database — is alive, restarts nothing, and is silently not
//! doing its job. This is the state that makes that visible.
//!
//! Atomics rather than a lock: the writers are two background loops and the
//! reader is a health probe, so a torn read has no consequence beyond a health
//! response that is one tick stale.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// Live scheduler state, shared between the loops and `/health`.
///
/// `Default` is written out rather than derived because one field's "no value"
/// sentinel is not zero: a derived default would leave `oldest_pending_secs` at
/// `0`, which reads back as *"the oldest waiting occurrence is zero seconds
/// old"* rather than *"nothing is waiting"* — a permanently-fresh backlog on
/// every node that has never scheduled anything.
#[derive(Debug)]
pub struct CronStatus {
    /// Unix seconds of the last reconciliation pass that completed without an
    /// error. `0` means "none since boot".
    last_reconcile_ok: AtomicI64,
    /// Unix seconds of the last claim query that succeeded — including one that
    /// found nothing, which is a healthy answer.
    last_claim_ok: AtomicI64,
    /// Lease renewals that failed since boot. Each one is an attempt that was
    /// cancelled mid-run.
    renewal_failures: AtomicU64,
    /// Set while a database call is failing, cleared on the next success. This
    /// is the fail-closed signal: while it is set, nothing is scheduled and
    /// nothing is claimed.
    db_unavailable: AtomicBool,
    /// Age in seconds of the oldest occurrence waiting for a worker, as of the
    /// last pass. `-1` means nothing was waiting.
    oldest_pending_secs: AtomicI64,
}

impl Default for CronStatus {
    fn default() -> Self {
        Self {
            last_reconcile_ok: AtomicI64::new(0),
            last_claim_ok: AtomicI64::new(0),
            renewal_failures: AtomicU64::new(0),
            db_unavailable: AtomicBool::new(false),
            // -1, not 0: see the type's doc comment.
            oldest_pending_secs: AtomicI64::new(-1),
        }
    }
}

impl CronStatus {
    pub fn new() -> Self {
        Self::default()
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    pub fn record_reconcile_ok(&self) {
        self.last_reconcile_ok.store(Self::now(), Ordering::Relaxed);
        self.db_unavailable.store(false, Ordering::Relaxed);
    }

    pub fn record_claim_ok(&self) {
        self.last_claim_ok.store(Self::now(), Ordering::Relaxed);
        self.db_unavailable.store(false, Ordering::Relaxed);
    }

    pub fn record_db_unavailable(&self) {
        self.db_unavailable.store(true, Ordering::Relaxed);
    }

    pub fn record_renewal_failure(&self) {
        self.renewal_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_oldest_pending_secs(&self, secs: Option<i64>) {
        self.oldest_pending_secs
            .store(secs.unwrap_or(-1), Ordering::Relaxed);
    }

    pub fn last_reconcile_ok(&self) -> Option<i64> {
        match self.last_reconcile_ok.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(secs),
        }
    }

    pub fn last_claim_ok(&self) -> Option<i64> {
        match self.last_claim_ok.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(secs),
        }
    }

    pub fn renewal_failures(&self) -> u64 {
        self.renewal_failures.load(Ordering::Relaxed)
    }

    pub fn db_unavailable(&self) -> bool {
        self.db_unavailable.load(Ordering::Relaxed)
    }

    pub fn oldest_pending_secs(&self) -> Option<i64> {
        match self.oldest_pending_secs.load(Ordering::Relaxed) {
            secs if secs < 0 => None,
            secs => Some(secs),
        }
    }

    /// Seconds since the last successful reconciliation, or `None` when there
    /// has not been one since boot.
    pub fn reconcile_age_secs(&self) -> Option<i64> {
        self.last_reconcile_ok()
            .map(|at| Ord::max(Self::now() - at, 0))
    }

    /// Whether the scheduler is failing to do its job, given how often it is
    /// supposed to run.
    ///
    /// Not simply "an error happened": a single failed pass is retried a second
    /// later and is not worth a health signal. What matters is a pass that has
    /// not *succeeded* for long enough that occurrences are now being missed —
    /// so the threshold is derived from the poll interval rather than fixed.
    ///
    /// A node that has never completed a pass is not degraded either: on a
    /// freshly started process the first pass has simply not happened yet, and
    /// reporting `degraded` for the first second of every boot would train
    /// operators to ignore this.
    pub fn is_degraded(&self, poll_interval: std::time::Duration) -> bool {
        if self.db_unavailable() {
            return true;
        }
        let Some(age) = self.reconcile_age_secs() else {
            return false;
        };
        age > Ord::max(poll_interval.as_secs().saturating_mul(10), 60) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_fresh_scheduler_is_not_degraded() {
        let status = CronStatus::new();
        assert_eq!(status.last_reconcile_ok(), None);
        assert!(
            !status.is_degraded(Duration::from_secs(1)),
            "a node whose first pass has not run yet is starting, not broken"
        );
    }

    #[test]
    fn a_recent_pass_is_healthy() {
        let status = CronStatus::new();
        status.record_reconcile_ok();
        assert!(!status.is_degraded(Duration::from_secs(1)));
        assert_eq!(status.reconcile_age_secs(), Some(0));
    }

    /// The failure the supervisor cannot see: the loop is alive and every pass
    /// is erroring.
    #[test]
    fn a_stale_pass_is_degraded_against_the_poll_interval() {
        let status = CronStatus::new();
        status
            .last_reconcile_ok
            .store(CronStatus::now() - 120, Ordering::Relaxed);
        assert!(
            status.is_degraded(Duration::from_secs(1)),
            "two minutes without a pass on a one-second interval is broken"
        );
        // The threshold scales with the interval, with a floor: a node polling
        // every minute is not degraded two minutes in.
        assert!(!status.is_degraded(Duration::from_secs(60)));
    }

    #[test]
    fn a_database_failure_degrades_immediately_and_recovers_on_success() {
        let status = CronStatus::new();
        status.record_reconcile_ok();
        status.record_db_unavailable();
        assert!(status.is_degraded(Duration::from_secs(1)));

        status.record_claim_ok();
        assert!(!status.is_degraded(Duration::from_secs(1)));
    }

    /// "Nothing is waiting" and "something has been waiting for zero seconds"
    /// are different answers, and a fresh node must give the first one.
    #[test]
    fn the_backlog_gauge_distinguishes_empty_from_zero_age() {
        let status = CronStatus::new();
        assert_eq!(status.oldest_pending_secs(), None);
        status.set_oldest_pending_secs(Some(0));
        assert_eq!(status.oldest_pending_secs(), Some(0));
        status.set_oldest_pending_secs(None);
        assert_eq!(status.oldest_pending_secs(), None);
    }

    #[test]
    fn renewal_failures_accumulate() {
        let status = CronStatus::new();
        status.record_renewal_failure();
        status.record_renewal_failure();
        assert_eq!(status.renewal_failures(), 2);
    }
}
