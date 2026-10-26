//! The deployment store's artifact collector, scheduled.
//!
//! Every definition, connection configuration and connector-package file a
//! deployment can name is an `artifact_objects` row, written once by digest.
//! Nothing removed them, so the table grew with every upload for the life of
//! the database. This job runs the store's two-phase collection
//! (`ImmutableDeploymentStore::mark_and_collect_garbage_in_db`, which also
//! states what is never collected) every `storage.artifact_gc_interval_secs`.
//!
//! It is a retention job like trace and audit cleanup, and shares their body:
//! `Optional` under the supervisor (a node that has stopped collecting still
//! serves correctly), the first immediate tick skipped, and in a cluster one
//! node per tick through the `deployment_gc` job lease. The lease saves work,
//! not correctness: a pass is one transaction behind the deployment head's
//! lock, so two passes on two nodes would serialise, not interleave.

use std::sync::Arc;

use crate::runtime::state::AppState;

/// The task, job-lease and job-metric name.
pub const JOB: &str = "deployment_gc";

/// Start the collector, unless `storage.artifact_gc_interval_secs` is `0`.
pub fn start(state: &AppState) {
    let interval_secs = state.config.storage.artifact_gc_interval_secs;
    if interval_secs == 0 {
        tracing::info!("Deployment artifact collection disabled (artifact_gc_interval_secs = 0)");
        return;
    }
    if state.tasks.is_registered(JOB) {
        return;
    }
    let lease_gate = state.cluster.enabled.then(|| {
        Arc::new(crate::cluster::JobLeaseGate::new(
            state.cluster.repo.clone(),
            state.cluster.instance_id.clone(),
        ))
    });
    let pass_state = state.clone();
    crate::queue::supervise_retention_job(
        &state.tasks,
        JOB,
        interval_secs,
        lease_gate,
        move || {
            let state = pass_state.clone();
            async move { collect_once(&state).await }
        },
        |outcome| {
            if let Err(e) = outcome {
                tracing::error!(error = %e, "Deployment artifact collection failed");
            }
        },
    );
}

/// One collection pass: what it drained and deleted is logged and counted.
/// Answers how many artifacts it deleted.
pub async fn collect_once(state: &AppState) -> Result<u64, crate::errors::OrionError> {
    let storage = &state.config.storage;
    let outcome = state
        .deployments
        .mark_and_collect_garbage_in_db(
            &state.db_pool,
            storage.deployment_retain_revisions as usize,
            storage.artifact_gc_grace_secs,
        )
        .await
        .map_err(|e| crate::errors::OrionError::internal(format!("artifact collection: {e}")))?;
    crate::metrics::record_deployment_artifacts_collected(
        outcome.draining.len() as u64,
        outcome.deleted.len() as u64,
        outcome.deleted_bytes,
    );
    if !outcome.is_empty() {
        tracing::info!(
            draining = outcome.draining.len(),
            deleted = outcome.deleted.len(),
            deleted_bytes = outcome.deleted_bytes,
            revived = outcome.revived.len(),
            compacted_revisions = outcome.compacted.len(),
            "Deployment artifact collection pass"
        );
    }
    Ok(outcome.deleted.len() as u64)
}
