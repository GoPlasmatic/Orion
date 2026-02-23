use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::repositories::jobs::JobRepository;

/// A message submitted to the job queue for async processing.
pub struct QueueMessage {
    pub job_id: String,
    pub channel: String,
    pub payload: Value,
    pub metadata: Value,
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
        // Wait for the dispatcher to finish (all in-flight work will complete)
        let _ = self.join_handle.await;
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
            process_job(msg, engine, job_repo).await;
            drop(permit);
        });
    }

    // Wait for all in-flight jobs to complete
    let _ = semaphore.acquire_many(max_workers as u32).await;
    tracing::info!("Job queue workers shut down");
}

/// Process a single queued job.
async fn process_job(
    msg: QueueMessage,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    job_repo: Arc<dyn JobRepository>,
) {
    let job_id = msg.job_id;
    let channel = msg.channel;
    let start = Instant::now();

    tracing::info!(job_id = %job_id, channel = %channel, "Processing job");

    // Mark as running
    if let Err(e) = job_repo.update_status(&job_id, "running", None, None).await {
        tracing::error!(job_id = %job_id, error = %e, "Failed to update job status to running");
        return;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    if let Some(meta_obj) = msg.metadata.as_object() {
        for (k, v) in meta_obj {
            message.metadata_mut()[k] = v.clone();
        }
    }

    // Process through engine
    let engine_guard = engine.read().await;
    let result = engine_guard
        .process_message_for_channel(&channel, &mut message)
        .await;
    drop(engine_guard);

    let duration = start.elapsed().as_secs_f64();

    match result {
        Ok(()) => {
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration);

            let result_json = serde_json::to_string(&serde_json::json!({
                "id": message.id,
                "data": message.data(),
            }))
            .unwrap_or_default();

            let _ = job_repo.set_result(&job_id, &result_json).await;
            let _ = job_repo
                .update_status(&job_id, "completed", None, Some(1))
                .await;
        }
        Err(e) => {
            metrics::record_message(&channel, "error");
            metrics::record_error("engine");

            let _ = job_repo
                .update_status(&job_id, "failed", Some(&e.to_string()), None)
                .await;
        }
    }
}
