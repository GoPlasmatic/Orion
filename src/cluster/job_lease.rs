//! Acquire-per-tick single-flight gate for background jobs (A7).
//!
//! No leader election: each job's worker attempts to acquire/renew a named
//! lease in `job_leases` before every tick and skips the tick when another
//! node holds it. The incumbent renews cheaply; a dead holder's lease
//! expires one TTL later and any node takes over. TTLs must exceed the
//! job's tick interval so the incumbent renews before expiry.

use std::sync::Arc;

use crate::storage::repositories::cluster::ClusterRepository;

pub struct JobLeaseGate {
    repo: Arc<dyn ClusterRepository>,
    holder: String,
}

impl JobLeaseGate {
    pub fn new(repo: Arc<dyn ClusterRepository>, holder: String) -> Self {
        Self { repo, holder }
    }

    /// True when this node holds the lease after the call. DB errors count
    /// as "not held" (skip the tick, warn) — duplicate-avoidance must never
    /// turn a DB blip into duplicated work by guessing.
    pub async fn try_acquire(&self, job: &str, ttl_secs: u64) -> bool {
        match self
            .repo
            .try_acquire_job_lease(job, &self.holder, ttl_secs)
            .await
        {
            Ok(held) => held,
            Err(e) => {
                crate::metrics::record_error("job_lease");
                tracing::warn!(job, error = %e, "Job lease check failed; skipping tick");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::OrionError;
    use crate::storage::repositories::cluster::EpochRow;
    use async_trait::async_trait;

    /// Stub repository whose lease call answers a canned result (T15). The
    /// other trait methods are unreachable from `JobLeaseGate`.
    struct StubClusterRepo {
        lease_result: fn() -> Result<bool, OrionError>,
    }

    #[async_trait]
    impl ClusterRepository for StubClusterRepo {
        async fn bump_epoch(&self) -> Result<i64, OrionError> {
            unreachable!("not exercised by JobLeaseGate")
        }
        async fn get_epoch(&self) -> Result<EpochRow, OrionError> {
            unreachable!("not exercised by JobLeaseGate")
        }
        async fn request_breaker_reset(&self, _key: &str) -> Result<i64, OrionError> {
            unreachable!("not exercised by JobLeaseGate")
        }
        async fn try_acquire_job_lease(
            &self,
            _job_name: &str,
            _holder: &str,
            _ttl_secs: u64,
        ) -> Result<bool, OrionError> {
            (self.lease_result)()
        }
    }

    fn gate(lease_result: fn() -> Result<bool, OrionError>) -> JobLeaseGate {
        JobLeaseGate::new(Arc::new(StubClusterRepo { lease_result }), "node-a".into())
    }

    /// The invariant the whole single-flight design rests on: a DB error must
    /// answer "not held", never "held". Guessing "held" on a blip is how two
    /// nodes end up running the same cleanup tick.
    #[tokio::test]
    async fn a_db_error_is_not_held() {
        let g = gate(|| Err(OrionError::internal("connection reset")));
        assert!(
            !g.try_acquire("trace_cleanup", 120).await,
            "a DB error must skip the tick, not duplicate it"
        );
    }

    #[tokio::test]
    async fn the_repository_answer_passes_through() {
        assert!(gate(|| Ok(true)).try_acquire("trace_cleanup", 120).await);
        assert!(!gate(|| Ok(false)).try_acquire("trace_cleanup", 120).await);
    }
}
