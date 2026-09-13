//! The model admission worker: the loop that turns a queued job into a
//! verdict on the row.
//!
//! `model::admission::admit` is a pure function over its dependencies; what
//! it does not know is where the storage connector comes from and where the
//! verdict goes. Both are answered here, with the node's live components:
//! the connector registry resolves `job.artifact.connector`, and the model
//! repository records what admission found. [`admit_now`] is that whole
//! sequence for one job, run inline — what the worker calls per job, and
//! what a test or a `?wait=true` request calls without a worker.

use serde_json::Value;

use crate::connector::ConnectorConfig;
use crate::errors::OrionError;
use crate::model::admission::{
    AdmissionDeps, AdmissionJob, AdmissionOutcome, AdmissionState, admission_json, admit,
    run_worker,
};
use crate::model::{ArtifactRef, Manifest};
use crate::runtime::{Criticality, TaskRegistry};
use crate::server::state::AppState;
use crate::storage::models::Model;

/// The supervised task's name, as `/health` and `/readyz` report it.
pub const TASK_NAME: &str = "model_admission";

/// The admission job for a stored version: the row's manifest, artifact
/// reference and signature, addressed by id and version. The manifest is
/// decoded, not re-validated — the row holds the validated form, and a
/// rule added since it was written is preflight's to report, not a reason
/// for the node to stop admitting.
pub fn job_for(row: &Model) -> Result<AdmissionJob, OrionError> {
    let artifact: ArtifactRef =
        serde_json::from_str(&row.artifact_json).map_err(|e| OrionError::Internal {
            context: format!(
                "model '{}' version {}: artifact_json does not parse: {e}",
                row.model_id, row.version
            ),
            source: None,
        })?;
    let manifest: Manifest =
        serde_json::from_str(&row.manifest_json).map_err(|e| OrionError::Internal {
            context: format!(
                "model '{}' version {}: manifest_json does not parse: {e}",
                row.model_id, row.version
            ),
            source: None,
        })?;
    Ok(AdmissionJob {
        model_id: row.model_id.clone(),
        version: row.version,
        artifact,
        signature: row.signature.clone(),
        manifest,
    })
}

/// Admit `job` on this node and record the verdict on its row.
///
/// The connector the job names must be a `storage` connector in the live
/// registry; anything else is a verdict too — `failed` at stage `gate` —
/// because the row is what an operator reads, and a job that vanished
/// without one is the silent failure this worker exists to avoid. An `Err`
/// is a node that could not *record* the verdict (models disabled, a row
/// that no longer exists, a database that will not write), never an
/// artifact that failed admission.
pub async fn admit_now(
    state: &AppState,
    job: AdmissionJob,
) -> Result<AdmissionOutcome, OrionError> {
    let Some(models) = &state.models else {
        return Err(OrionError::validation(
            "models are disabled on this node (models.enabled = false), so nothing can be \
             admitted here",
        ));
    };
    let storage = match state.connector_registry.get(&job.artifact.connector).await {
        Some(config) => match config.as_ref() {
            ConnectorConfig::Storage(storage) => Some(storage.clone()),
            other => {
                return record(
                    state,
                    &models.node,
                    gate_failure(
                        &job,
                        format!(
                            "connector '{}' is a {} connector, and a model artifact is read \
                             through a storage connector",
                            job.artifact.connector,
                            other.connector_type().as_str()
                        ),
                    ),
                )
                .await;
            }
        },
        None => None,
    };
    let Some(storage) = storage else {
        return record(
            state,
            &models.node,
            gate_failure(
                &job,
                format!(
                    "connector '{}' is not loaded on this node — it does not exist, is \
                     disabled, or failed to load (see /health)",
                    job.artifact.connector
                ),
            ),
        )
        .await;
    };
    let deps = AdmissionDeps {
        store: &models.store,
        storage: &storage,
        client: &state.http_client,
        config: &state.config.models,
        node: &models.node,
        runtimes: &models.runtimes,
    };
    let outcome = admit(&deps, &job).await;
    record(state, &models.node, outcome).await
}

/// A verdict the sequence never ran for: the connector gate refused the job
/// before a byte moved.
fn gate_failure(job: &AdmissionJob, reason: String) -> AdmissionOutcome {
    crate::metrics::record_model_admission("failed", Some("gate"), 0.0);
    AdmissionOutcome {
        model_id: job.model_id.clone(),
        version: job.version,
        state: AdmissionState::Failed {
            stage: "gate",
            reason,
        },
        artifact_path: None,
        elapsed: std::time::Duration::ZERO,
    }
}

/// Write the verdict — and, on a pass, the stats — to the row.
async fn record(
    state: &AppState,
    node: &str,
    outcome: AdmissionOutcome,
) -> Result<AdmissionOutcome, OrionError> {
    let verdict = admission_json(&outcome, node, chrono::Utc::now());
    state
        .repos
        .models
        .set_admission(&outcome.model_id, outcome.version, &verdict.to_string())
        .await?;
    let stats: Option<Value> = match &outcome.state {
        AdmissionState::Passed { stats } => Some(serde_json::to_value(stats)?),
        AdmissionState::Failed { .. } => None,
    };
    state
        .repos
        .models
        .set_stats(
            &outcome.model_id,
            outcome.version,
            stats.map(|s| s.to_string()).as_deref(),
        )
        .await?;
    match &outcome.state {
        AdmissionState::Passed { .. } => tracing::info!(
            model = %outcome.model_id,
            version = outcome.version,
            elapsed_ms = outcome.elapsed.as_millis() as u64,
            "Model admitted"
        ),
        AdmissionState::Failed { stage, reason } => tracing::warn!(
            model = %outcome.model_id,
            version = outcome.version,
            stage,
            reason,
            "Model admission failed"
        ),
    }
    Ok(outcome)
}

/// Start the worker under `tasks`, draining the queue `state.models` holds.
///
/// Each job runs on its own task, so a panic inside one admission is a
/// logged error and the next job is picked up; the loop itself ends only at
/// shutdown, because the application state keeps a sender for as long as it
/// lives. Nothing to start when models are disabled.
pub fn start(tasks: &TaskRegistry, state: AppState) {
    let Some(models) = state.models.clone() else {
        return;
    };
    tasks.supervise(TASK_NAME, Criticality::Required, move |mut shutdown| {
        let state = state.clone();
        let models = models.clone();
        async move {
            let Some(receiver) = models.take_receiver() else {
                // The receiver went with an earlier attempt, which cannot
                // happen while every job runs on its own task; if it does,
                // the supervisor's restart loop keeps `/health` honest.
                tracing::error!(
                    task = TASK_NAME,
                    "the admission queue's receiver is gone; the worker cannot restart"
                );
                return;
            };
            let worker = run_worker(receiver, move |job| {
                let state = state.clone();
                async move {
                    let model = job.model_id.clone();
                    let version = job.version;
                    let run = tokio::spawn(async move {
                        if let Err(e) = admit_now(&state, job).await {
                            tracing::error!(
                                model = %model,
                                version,
                                error = %e,
                                "Model admission could not be recorded"
                            );
                        }
                    });
                    if let Err(e) = run.await {
                        tracing::error!(error = %e, "Model admission task died");
                        crate::metrics::record_error("model_admission");
                    }
                }
            });
            tokio::select! {
                _ = worker => {}
                _ = shutdown.signalled() => {}
            }
        }
    });
}
