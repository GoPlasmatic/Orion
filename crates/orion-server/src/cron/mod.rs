//! Scheduled channels: the runtime behind `protocol: "cron"`.
//!
//! Two supervised loops over one durable ledger.
//!
//! ```text
//!    active cron channels in the runtime generation
//!                      |
//!                      v
//!            reconciler  (scheduler.rs)
//!         plan a pass, materialise what is due
//!                      |
//!                      v
//!              cron_occurrences  ── the durable ledger
//!                      |
//!            claim + singleton, one transaction
//!                      v
//!               workers  (worker.rs)
//!         guards -> execute_admitted -> trace + settle
//! ```
//!
//! The split is the design. The reconciler decides *what should happen* and
//! writes rows; the workers decide *what is happening now* and run them.
//! Neither holds state the other needs, so either can die, restart, or run on
//! a different node without coordination beyond the rows themselves.
//!
//! **Where the schedule lives.** Not here: a schedule is authored content on a
//! cron channel, compiled by `ChannelLoader` into a
//! [`CronDescriptor`](crate::channel::CronDescriptor) and carried on the
//! runtime generation like every other per-channel setting. This module reads
//! it from the generation it loads, so the schedules it plans against and the
//! engine that runs them are always one build.
//!
//! **What is guaranteed, and what is not.** Occurrences are durable and
//! at-least-once; a `forbid` singleton is non-overlapping across the cluster
//! for as long as the shared database is reachable. Neither of those is
//! exactly-once *side effects*: a worker that loses its lease cancels, but a
//! connector call already in flight cannot be recalled, and no distributed
//! lease can prove an old holder stopped. Scheduled work that must not be
//! applied twice needs an idempotent destination.

pub mod metadata;
pub mod scheduler;
pub mod status;
pub mod worker;

use std::sync::Arc;

pub use scheduler::{ReconcileDeps, reconcile_once, run_reconcile};
pub use status::CronStatus;
pub use worker::{WorkerDeps, run_worker};

/// Everything [`start`] needs, assembled once by bootstrap.
pub struct CronDeps {
    pub runtime: Arc<crate::runtime::RuntimeHandle>,
    pub repo: Arc<dyn crate::storage::repositories::cron::CronRepository>,
    pub trace_repo: Arc<dyn crate::storage::repositories::traces::TraceSink>,
    pub persistence_queue: crate::queue::TracePersistenceQueue,
    pub global_trace_storage: crate::config::TraceStorageConfig,
    pub datalogic: Arc<dataflow_rs::datalogic_rs::Engine>,
    pub vars: Option<Arc<serde_json::Value>>,
    pub instance_id: String,
    pub status: Arc<CronStatus>,
    pub config: crate::config::CronConfig,
    pub max_result_size_bytes: usize,
    pub lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
}

/// Start the reconciler and the worker pool.
///
/// Both are `Required`. A node that has stopped scheduling still answers every
/// request correctly, which is exactly what makes this the wrong thing to
/// report as healthy: the schedules it declares are simply not running, and
/// nothing else on the instance would say so. The same argument the trace
/// dispatcher makes — a subsystem whose failure is invisible from the outside
/// has to fail loudly from the inside.
///
/// A node with `cron.enabled = false` starts neither, and any *active* cron
/// channel is quarantined at load with `components.cron` reporting `degraded`.
pub fn start(tasks: &crate::runtime::TaskRegistry, deps: CronDeps) {
    if !deps.config.enabled {
        tracing::info!("Cron scheduler disabled (cron.enabled = false)");
        return;
    }

    let CronDeps {
        runtime,
        repo,
        trace_repo,
        persistence_queue,
        global_trace_storage,
        datalogic,
        vars,
        instance_id,
        status,
        config,
        max_result_size_bytes,
        lease_gate,
    } = deps;

    let reconcile = Arc::new(ReconcileDeps {
        runtime: runtime.clone(),
        repo: repo.clone(),
        status: status.clone(),
        config: config.clone(),
        lease_gate,
    });
    // The `Arc` is what makes the body a factory: `supervise` re-runs it after a
    // failure, so nothing can be moved into it.
    tasks.supervise(
        "cron_reconcile",
        crate::runtime::Criticality::Required,
        move |shutdown| run_reconcile(reconcile.clone(), shutdown),
    );

    let worker = Arc::new(WorkerDeps {
        runtime,
        repo,
        trace_repo,
        persistence_queue,
        global_trace_storage,
        datalogic,
        vars,
        instance_id,
        status,
        config: config.clone(),
        max_result_size_bytes,
    });
    tasks.supervise(
        "cron_worker",
        crate::runtime::Criticality::Required,
        move |shutdown| run_worker(worker.clone(), shutdown),
    );

    tracing::info!(
        poll_interval_ms = config.poll_interval_ms,
        workers = config.workers,
        claim_batch_size = config.claim_batch_size,
        "Cron scheduler started"
    );
}

/// Age terminal occurrences out on the same cadence as trace cleanup.
///
/// The same knob deliberately: an occurrence and the trace it produced are two
/// halves of one record, and letting them expire on different schedules would
/// leave an operator reading occurrences whose traces are gone, or the reverse.
///
/// `Optional`, like the other retention jobs: a node that has stopped expiring
/// old rows still schedules and runs everything correctly, so this is a
/// `/health` degradation rather than a reason to take the node out of rotation.
///
/// Runs whether or not the scheduler is enabled — a node with `cron.enabled =
/// false` still shares the database, and history still has to age out.
pub fn start_cleanup(
    tasks: &crate::runtime::TaskRegistry,
    retention_hours: u64,
    interval_secs: u64,
    repo: Arc<dyn crate::storage::repositories::cron::CronRepository>,
    lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
) {
    if retention_hours == 0 {
        return;
    }
    crate::queue::supervise_retention_job(
        tasks,
        "cron_cleanup",
        interval_secs,
        lease_gate,
        move || {
            let repo = repo.clone();
            async move { repo.delete_terminal_older_than(retention_hours).await }
        },
        move |outcome| match outcome {
            Ok(count) if count > 0 => {
                tracing::info!(
                    deleted = count,
                    retention_hours,
                    "Cron occurrence cleanup completed"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "Cron occurrence cleanup failed"),
        },
    );
}
