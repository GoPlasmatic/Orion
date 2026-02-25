use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::jobs::JobRepository;

/// A message submitted to the job queue for async processing.
pub struct QueueMessage {
    pub job_id: String,
    pub channel: String,
    pub payload: Value,
    pub metadata: Value,
    /// Serialized W3C trace context headers captured at submission time.
    /// Used to link async job processing spans back to the originating request.
    #[cfg(feature = "otel")]
    pub trace_headers: std::collections::HashMap<String, String>,
}

/// In-memory job queue backed by a tokio mpsc channel.
///
/// Jobs are submitted via `submit()` and processed by a semaphore-limited
/// worker pool that runs in the background.
#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<QueueMessage>,
}

impl JobQueue {
    /// Submit a job to the queue for background processing.
    pub async fn submit(&self, msg: QueueMessage) -> Result<(), crate::errors::OrionError> {
        self.sender
            .send(msg)
            .await
            .map_err(|_| crate::errors::OrionError::Internal("Job queue is closed".to_string()))
    }
}

/// Handle returned from `start_workers` to manage the worker lifecycle.
pub struct WorkerHandle {
    _sender: mpsc::Sender<QueueMessage>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl WorkerHandle {
    /// Gracefully shut down the worker pool.
    ///
    /// Drops the sender (the JobQueue clone also holds one), so call this
    /// only after the HTTP server has stopped accepting new requests.
    /// The returned future resolves when all in-flight jobs are complete.
    pub async fn shutdown(self) {
        drop(self._sender);
        // Wait for the dispatcher with a timeout to prevent hanging on stuck jobs
        if tokio::time::timeout(Duration::from_secs(30), self.join_handle)
            .await
            .is_err()
        {
            tracing::warn!(
                "Job queue workers did not shut down within 30s timeout, proceeding with exit"
            );
        }
    }
}

/// Start the background worker pool and return a (JobQueue, WorkerHandle) pair.
///
/// `max_workers` controls the maximum number of concurrent jobs.
/// `buffer_size` controls the channel buffer (pending jobs waiting to be picked up).
pub fn start_workers(
    max_workers: usize,
    buffer_size: usize,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    job_repo: Arc<dyn JobRepository>,
) -> (JobQueue, WorkerHandle) {
    let (tx, rx) = mpsc::channel::<QueueMessage>(buffer_size);

    let handle = tokio::spawn(dispatcher_loop(rx, max_workers, engine, job_repo));

    let queue = JobQueue { sender: tx.clone() };
    let worker_handle = WorkerHandle {
        _sender: tx,
        join_handle: handle,
    };

    (queue, worker_handle)
}

/// Main dispatcher loop: receives jobs from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent jobs.
async fn dispatcher_loop(
    mut rx: mpsc::Receiver<QueueMessage>,
    max_workers: usize,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    job_repo: Arc<dyn JobRepository>,
) {
    let semaphore = Arc::new(Semaphore::new(max_workers));

    while let Some(msg) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        let engine = engine.clone();
        let job_repo = job_repo.clone();

        tokio::spawn(async move {
            let _permit = permit; // guard: dropped on scope exit, even on panic
            process_job(msg, engine, job_repo).await;
        });
    }

    // Wait for all in-flight jobs to complete, with a timeout
    if tokio::time::timeout(
        Duration::from_secs(30),
        semaphore.acquire_many(max_workers as u32),
    )
    .await
    .is_err()
    {
        tracing::warn!("Timed out waiting for in-flight jobs to complete");
    }
    tracing::info!("Job queue workers shut down");
}

/// Process a single queued job.
#[tracing::instrument(skip(msg, engine, job_repo), fields(job_id = %msg.job_id, channel = %msg.channel))]
async fn process_job(
    msg: QueueMessage,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    job_repo: Arc<dyn JobRepository>,
) {
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
    #[cfg(feature = "otel")]
    {
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        struct MapExtractor<'a>(&'a std::collections::HashMap<String, String>);
        impl opentelemetry::propagation::Extractor for MapExtractor<'_> {
            fn get(&self, key: &str) -> Option<&str> {
                self.0.get(key).map(|v| v.as_str())
            }
            fn keys(&self) -> Vec<&str> {
                self.0.keys().map(|k| k.as_str()).collect()
            }
        }

        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&MapExtractor(&msg.trace_headers));
        tracing::Span::current().set_parent(cx);
    }

    let job_id = msg.job_id;
    let channel = msg.channel;
    let start = Instant::now();

    // Mark as running
    if let Err(e) = job_repo
        .update_status(&job_id, models::JOB_STATUS_RUNNING, None, None)
        .await
    {
        tracing::error!(job_id = %job_id, error = %e, "Failed to update job status to running");
        return;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    crate::engine::utils::merge_metadata(&mut message, &msg.metadata);

    // Clone the inner Arc<Engine> and release the lock immediately
    let engine_ref = engine.read().await.clone();
    let result = engine_ref
        .process_message_for_channel(&channel, &mut message)
        .await;

    let duration = start.elapsed().as_secs_f64();

    match result {
        Ok(()) => {
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration);

            let result_json = match serde_json::to_string(&serde_json::json!({
                "id": message.id,
                "data": message.data(),
            })) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!(job_id = %job_id, error = %e, "Failed to serialize job result");
                    if let Err(db_err) = job_repo
                        .update_status(
                            &job_id,
                            models::JOB_STATUS_FAILED,
                            Some(&format!("Result serialization failed: {e}")),
                            None,
                        )
                        .await
                    {
                        tracing::error!(job_id = %job_id, error = %db_err, "Failed to update job status to failed after serialization error");
                    }
                    return;
                }
            };

            let mut result_saved = false;
            for attempt in 0..3 {
                match job_repo.set_result(&job_id, &result_json).await {
                    Ok(_) => {
                        result_saved = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_id = %job_id, error = %e, attempt = attempt + 1,
                            "Failed to save job result, retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
                    }
                }
            }

            if result_saved {
                if let Err(e) = job_repo
                    .update_status(&job_id, models::JOB_STATUS_COMPLETED, None, Some(1))
                    .await
                {
                    tracing::error!(job_id = %job_id, error = %e, "Failed to mark job as completed");
                }
            } else {
                tracing::error!(job_id = %job_id, "Failed to save job result after 3 attempts, marking as failed");
                if let Err(db_err) = job_repo
                    .update_status(
                        &job_id,
                        models::JOB_STATUS_FAILED,
                        Some("Result persistence failed after retries"),
                        None,
                    )
                    .await
                {
                    tracing::error!(job_id = %job_id, error = %db_err, "Failed to update job status to failed after persistence failure");
                }
            }
        }
        Err(e) => {
            metrics::record_message(&channel, "error");
            metrics::record_error("engine");

            if let Err(db_err) = job_repo
                .update_status(
                    &job_id,
                    models::JOB_STATUS_FAILED,
                    Some(&e.to_string()),
                    None,
                )
                .await
            {
                tracing::error!(job_id = %job_id, error = %db_err, "Failed to mark job as failed");
            }
        }
    }
}
