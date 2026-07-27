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
    use super::*;

    use crate::storage::DbPool;
    use crate::storage::repositories::audit_logs::SqlAuditLogRepository;
    use crate::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};

    /// One expired row, ready to be collected by the next tick.
    async fn pool_with_expired_row() -> DbPool {
        let pool = crate::storage::test_sqlite_pool().await;
        let repo = SqlAuditLogRepository::new(pool.clone());
        repo.insert("admin...", "create", "workflow", "old", None)
            .await
            .expect("insert");
        crate::storage::repositories::audit_logs::tests::backdate(&pool, "old", 120).await;
        pool
    }

    async fn remaining(pool: &DbPool) -> i64 {
        SqlAuditLogRepository::new(pool.clone())
            .count()
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn test_disabled_when_retention_is_zero() {
        let pool = pool_with_expired_row().await;
        let handle = start_audit_cleanup(
            0,
            1,
            Arc::new(SqlAuditLogRepository::new(pool.clone())),
            None,
        );
        assert!(handle.is_none(), "0 days must mean retain forever");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(remaining(&pool).await, 1);
    }

    #[tokio::test]
    async fn test_ungated_job_deletes_expired_rows() {
        let pool = pool_with_expired_row().await;
        let handle = start_audit_cleanup(
            90,
            1,
            Arc::new(SqlAuditLogRepository::new(pool.clone())),
            None,
        )
        .expect("job started");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        handle.abort();
        assert_eq!(remaining(&pool).await, 0);
    }

    #[tokio::test]
    async fn test_job_skips_tick_while_another_node_holds_the_lease() {
        let pool = pool_with_expired_row().await;
        let cluster_repo: Arc<dyn ClusterRepository> =
            Arc::new(SqlClusterRepository::new(pool.clone()));

        // A peer takes the lease first and holds it well past the tick.
        assert!(
            cluster_repo
                .try_acquire_job_lease("audit_cleanup", "node-a", 600)
                .await
                .expect("peer lease"),
        );

        let gate = Arc::new(crate::cluster::JobLeaseGate::new(
            cluster_repo.clone(),
            "node-b".to_string(),
        ));
        let handle = start_audit_cleanup(
            90,
            1,
            Arc::new(SqlAuditLogRepository::new(pool.clone())),
            Some(gate),
        )
        .expect("job started");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        handle.abort();
        assert_eq!(
            remaining(&pool).await,
            1,
            "the lease holder is node-a, so node-b must not delete"
        );
    }
}
