//! Retention cleanup for the `audit_logs` table (D2).
//!
//! Every admin mutation writes an audit row and nothing ever removed one, so
//! the table grew for the lifetime of the deployment. This mirrors the trace
//! cleanup job: a periodic lease-gated DELETE of rows past
//! `audit.retention_days`.

use std::sync::Arc;

use crate::storage::repositories::audit_logs::AuditLogRepository;

/// Start a background task that periodically deletes old audit-log entries.
///
/// Registered with the supervisor, which restarts it after a capped backoff
/// if it ever stops early.
/// If `retention_days` is 0, no cleanup task is started (retain forever).
///
/// `lease_gate` (cluster mode) single-flights the job: without it every
/// replica issues the same DELETE every tick. `None` on a single node.
pub fn start_audit_cleanup(
    tasks: &crate::runtime::TaskRegistry,
    retention_days: u64,
    interval_secs: u64,
    audit_repo: Arc<dyn AuditLogRepository>,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
) {
    if retention_days == 0 {
        tracing::info!("Audit log retention disabled (audit.retention_days = 0)");
        return;
    }

    super::supervise_retention_job(
        tasks,
        "audit_cleanup",
        interval_secs,
        lease_gate,
        // Cloned inside the closure: `async_trait` ties the returned future to
        // `&self`, so the borrow has to live in the future, not the closure.
        move || {
            let repo = audit_repo.clone();
            async move { repo.delete_older_than(retention_days).await }
        },
        move |outcome| match outcome {
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
        },
    );

    tracing::info!(
        retention_days = retention_days,
        interval_secs = interval_secs,
        "Audit log cleanup task started"
    );
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
    use std::time::Duration;

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
        async fn insert_tx(
            &self,
            _tx: &mut crate::storage::DbTransaction,
            _event: &crate::storage::repositories::audit_logs::AuditEvent,
        ) -> Result<(), OrionError> {
            unreachable!("this fake is only driven through the queue, which uses `insert`")
        }

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
        async fn bump_epoch(&self, _scope: &str) -> Result<i64, OrionError> {
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
        let tasks = crate::runtime::TaskRegistry::new();
        start_audit_cleanup(&tasks, 0, 1, repo.clone(), None);
        assert!(
            tasks.report().is_empty(),
            "0 days must mean retain forever — no job registered"
        );
        // Three ticks' worth of virtual time: a wrongly-started job would
        // have deleted by now.
        advance_and_yield(Duration::from_secs(3)).await;
        assert_eq!(repo.deletes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_ungated_job_deletes_expired_rows() {
        let repo = Arc::new(MockAuditRepo::default());
        let tasks = crate::runtime::TaskRegistry::new();
        start_audit_cleanup(&tasks, 90, 1, repo.clone(), None);

        // One advance consumes the skipped immediate tick, the next fires
        // the first real one.
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        assert!(
            repo.deletes.load(Ordering::SeqCst) >= 1,
            "the first interval tick must run the DELETE"
        );
        tasks.shutdown(Duration::from_secs(5)).await;
    }

    /// Run `job` under a thread-local Prometheus recorder on a paused
    /// current-thread runtime, and render what it recorded. The local
    /// recorder keeps the assertion immune to every other test in the
    /// binary sharing the global recorder.
    fn render_job_metrics<F, Fut>(job: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::set_enabled(true);
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("test runtime")
                .block_on(job());
        });
        handle.render()
    }

    /// O3: a successful tick must stamp `job_last_success_timestamp` — the
    /// gauge whose staleness is the only alertable signal that a cleanup
    /// loop is silently failing.
    #[test]
    fn test_successful_tick_stamps_the_job_health_gauge() {
        let out = render_job_metrics(|| async {
            let repo = Arc::new(MockAuditRepo::default());
            let tasks = crate::runtime::TaskRegistry::new();
            start_audit_cleanup(&tasks, 90, 1, repo, None);
            advance_and_yield(Duration::from_secs(1)).await;
            advance_and_yield(Duration::from_secs(1)).await;
            tasks.shutdown(Duration::from_secs(5)).await;
        });
        assert!(
            out.contains(r#"orion_job_last_success_timestamp_seconds{job="audit_cleanup"}"#),
            "a successful tick must stamp the job health gauge:\n{out}"
        );
    }

    /// The complement: a node that never wins the lease never succeeds, so
    /// its gauge must stay unstamped rather than lie about freshness.
    #[test]
    fn test_lease_refused_tick_does_not_stamp_the_gauge() {
        let out = render_job_metrics(|| async {
            let repo = Arc::new(MockAuditRepo::default());
            let gate = Arc::new(crate::cluster::JobLeaseGate::new(
                Arc::new(LeaseHeldElsewhere),
                "node-b".to_string(),
            ));
            let tasks = crate::runtime::TaskRegistry::new();
            start_audit_cleanup(&tasks, 90, 1, repo, Some(gate));
            advance_and_yield(Duration::from_secs(1)).await;
            advance_and_yield(Duration::from_secs(1)).await;
            tasks.shutdown(Duration::from_secs(5)).await;
        });
        assert!(
            !out.contains(r#"job="audit_cleanup""#),
            "a tick skipped for the lease is not a success:\n{out}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_job_skips_tick_while_another_node_holds_the_lease() {
        let repo = Arc::new(MockAuditRepo::default());
        let gate = Arc::new(crate::cluster::JobLeaseGate::new(
            Arc::new(LeaseHeldElsewhere),
            "node-b".to_string(),
        ));
        let tasks = crate::runtime::TaskRegistry::new();
        start_audit_cleanup(&tasks, 90, 1, repo.clone(), Some(gate));

        // Several ticks fire; every one must be refused by the gate.
        advance_and_yield(Duration::from_secs(1)).await;
        for _ in 0..3 {
            advance_and_yield(Duration::from_secs(1)).await;
        }
        tasks.shutdown(Duration::from_secs(5)).await;
        assert_eq!(
            repo.deletes.load(Ordering::SeqCst),
            0,
            "the lease holder is another node, so this one must not delete"
        );
    }
}
