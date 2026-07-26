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
