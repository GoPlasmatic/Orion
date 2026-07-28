//! Retention cleanup for the `audit_logs` table (D2).
//!
//! Every admin mutation writes an audit row and nothing ever removed one, so
//! the table grew for the lifetime of the deployment. This mirrors the trace
//! cleanup job: a periodic lease-gated DELETE of rows past
//! `queue.audit_retention_days`.

use std::sync::Arc;
use std::time::Duration;

use crate::storage::repositories::audit_logs::AuditLogRepository;

/// Start a background task that periodically deletes old audit-log entries.
///
/// Returns a `JoinHandle` that can be aborted on shutdown.
/// If `retention_days` is 0, no cleanup task is started (retain forever).
///
/// `lease_gate` (cluster mode) single-flights the job: without it every
/// replica issues the same DELETE every tick. `None` on a single node.
pub fn start_audit_cleanup(
    retention_days: u64,
    interval_secs: u64,
    audit_repo: Arc<dyn AuditLogRepository>,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if retention_days == 0 {
        tracing::info!("Audit log retention disabled (queue.audit_retention_days = 0)");
        return None;
    }

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick
        interval.tick().await;
        let lease_ttl = interval_secs + 60;

        loop {
            interval.tick().await;
            if let Some(ref gate) = lease_gate
                && !gate.try_acquire("audit_cleanup", lease_ttl).await
            {
                continue;
            }
            match audit_repo.delete_older_than(retention_days).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            deleted = count,
                            retention_days = retention_days,
                            "Audit log cleanup completed"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Audit log cleanup failed");
                }
            }
        }
    });

    tracing::info!(
        retention_days = retention_days,
        interval_secs = interval_secs,
        "Audit log cleanup task started"
    );

    Some(handle)
}

#[cfg(test)]
mod tests {
    //! Loop-scheduling semantics under a paused clock with MOCK repos (the
    //! dlq_retry.rs pattern). Real sqlx under `start_paused` is racy by
    //! construction: SQLite work happens on a non-tokio thread, and while
    //! the runtime parks waiting for it, auto-advance can burn through
    //! pool-acquire timeouts in virtual microseconds. The DELETE's SQL
    //! correctness is covered separately, timer-free, by the repository
    //! tests (`test_delete_older_than_removes_only_expired_rows`).

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::errors::OrionError;
    use crate::storage::models::AuditLogEntry;
    use crate::storage::repositories::audit_logs::AuditLogFilter;
    use crate::storage::repositories::cluster::{ClusterRepository, EpochRow};
    use crate::storage::repositories::helpers::PaginatedResult;

    /// Counts `delete_older_than` calls; never touches a database.
    #[derive(Default)]
    struct MockAuditRepo {
        deletes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AuditLogRepository for MockAuditRepo {
        async fn insert(
            &self,
            _principal: &str,
            _action: &str,
            _resource_type: &str,
            _resource_id: &str,
            _details: Option<&str>,
        ) -> Result<(), OrionError> {
            Ok(())
        }

        async fn list_paginated(
            &self,
            _filter: &AuditLogFilter,
        ) -> Result<PaginatedResult<AuditLogEntry>, OrionError> {
            Ok(PaginatedResult {
                data: vec![],
                total: 0,
                limit: 0,
                offset: 0,
            })
        }

        async fn delete_older_than(&self, _days: u64) -> Result<u64, OrionError> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }
    }

    /// A cluster repo whose job lease is always held by another node.
    struct LeaseHeldElsewhere;

    #[async_trait::async_trait]
    impl ClusterRepository for LeaseHeldElsewhere {
        async fn bump_epoch(&self) -> Result<i64, OrionError> {
            unreachable!("not used by audit cleanup")
        }

        async fn get_epoch(&self) -> Result<EpochRow, OrionError> {
            unreachable!("not used by audit cleanup")
        }

        async fn request_breaker_reset(&self, _key: &str) -> Result<i64, OrionError> {
            unreachable!("not used by audit cleanup")
        }

        async fn try_acquire_job_lease(
            &self,
            _job_name: &str,
            _holder: &str,
            _ttl_secs: u64,
        ) -> Result<bool, OrionError> {
            Ok(false)
        }
    }

    /// Advance the paused clock, then yield so the woken cleanup task can
    /// run its tick to completion (same pattern as dlq_retry.rs).
    async fn advance_and_yield(duration: Duration) {
        tokio::time::advance(duration).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_disabled_when_retention_is_zero() {
        let repo = Arc::new(MockAuditRepo::default());
        let handle = start_audit_cleanup(0, 1, repo.clone(), None);
        assert!(handle.is_none(), "0 days must mean retain forever");
        // Three ticks' worth of virtual time: a wrongly-started job would
        // have deleted by now.
        advance_and_yield(Duration::from_secs(3)).await;
        assert_eq!(repo.deletes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_ungated_job_deletes_expired_rows() {
        let repo = Arc::new(MockAuditRepo::default());
        let handle = start_audit_cleanup(90, 1, repo.clone(), None).expect("job started");

        // One advance consumes the skipped immediate tick, the next fires
        // the first real one.
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        assert!(
            repo.deletes.load(Ordering::SeqCst) >= 1,
            "the first interval tick must run the DELETE"
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_job_skips_tick_while_another_node_holds_the_lease() {
        let repo = Arc::new(MockAuditRepo::default());
        let gate = Arc::new(crate::cluster::JobLeaseGate::new(
            Arc::new(LeaseHeldElsewhere),
            "node-b".to_string(),
        ));
        let handle = start_audit_cleanup(90, 1, repo.clone(), Some(gate)).expect("job started");

        // Several ticks fire; every one must be refused by the gate.
        advance_and_yield(Duration::from_secs(1)).await;
        for _ in 0..3 {
            advance_and_yield(Duration::from_secs(1)).await;
        }
        handle.abort();
        assert_eq!(
            repo.deletes.load(Ordering::SeqCst),
            0,
            "the lease holder is another node, so this one must not delete"
        );
    }
}
