use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore, mpsc};

use crate::metrics;
use crate::storage::models;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

use super::QueueMessage;

/// Shared counters for queue observability metrics.
pub(super) struct QueueCounters {
    pub(super) pending: Arc<AtomicUsize>,
    pub(super) active: Arc<AtomicUsize>,
    pub(super) memory_bytes: Arc<AtomicUsize>,
}

/// Bundled context for the dispatcher loop, grouping parameters that share
/// the same lifecycle and reducing positional argument count.
pub(super) struct DispatcherContext {
    pub(super) max_workers: usize,
    pub(super) shutdown_timeout_secs: u64,
    pub(super) processing_timeout_ms: u64,
    pub(super) max_result_size_bytes: usize,
    pub(super) engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    pub(super) trace_repo: Arc<dyn TraceRepository>,
    pub(super) dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    pub(super) counters: QueueCounters,
}

/// Main dispatcher loop: receives traces from the channel and spawns processing
/// tasks, limited by a semaphore to `max_workers` concurrent traces.
pub(super) async fn dispatcher_loop(mut rx: mpsc::Receiver<QueueMessage>, ctx: DispatcherContext) {
    let semaphore = Arc::new(Semaphore::new(ctx.max_workers));

    while let Some(msg) = rx.recv().await {
        // Acquire a permit — blocks if all workers are busy
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

        // Estimate payload size for memory accounting
        let estimated_size = msg.payload.to_string().len() + msg.metadata.to_string().len();

        // Dequeued — decrement pending, increment active
        let pending = ctx
            .counters
            .pending
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        metrics::set_trace_queue_depth(pending as f64);
        let active = ctx.counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::set_trace_workers_active(active as f64);

        let engine = ctx.engine.clone();
        let trace_repo = ctx.trace_repo.clone();
        let dlq_repo = ctx.dlq_repo.clone();
        let active_counter = ctx.counters.active.clone();
        let memory_counter = ctx.counters.memory_bytes.clone();
        let processing_timeout_ms = ctx.processing_timeout_ms;
        let max_result_size_bytes = ctx.max_result_size_bytes;

        tokio::spawn(async move {
            let _permit = permit; // guard: dropped on scope exit, even on panic
            process_trace(
                msg,
                engine,
                trace_repo,
                dlq_repo,
                processing_timeout_ms,
                max_result_size_bytes,
            )
            .await;
            let active = active_counter
                .fetch_sub(1, Ordering::Relaxed)
                .saturating_sub(1);
            metrics::set_trace_workers_active(active as f64);
            // Release memory accounting
            let mem = memory_counter
                .fetch_sub(estimated_size, Ordering::Relaxed)
                .saturating_sub(estimated_size);
            metrics::set_trace_queue_memory_bytes(mem as f64);
        });
    }

    // Wait for all in-flight traces to complete, with a timeout
    if tokio::time::timeout(
        Duration::from_secs(ctx.shutdown_timeout_secs),
        semaphore.acquire_many(ctx.max_workers as u32),
    )
    .await
    .is_err()
    {
        tracing::warn!("Timed out waiting for in-flight traces to complete");
    }
    tracing::info!("Trace queue workers shut down");
}

/// Update trace status, logging an error if the DB call fails.
async fn set_trace_status(
    trace_repo: &dyn TraceRepository,
    trace_id: &str,
    status: &str,
    message: Option<&str>,
) {
    if let Err(e) = trace_repo.update_status(trace_id, status, message).await {
        tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to {}", status);
    }
}

/// Process a single queued trace.
#[tracing::instrument(skip(msg, engine, trace_repo, dlq_repo, processing_timeout_ms, max_result_size_bytes), fields(trace_id = %msg.trace_id, channel = %msg.channel))]
async fn process_trace(
    msg: QueueMessage,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    trace_repo: Arc<dyn TraceRepository>,
    dlq_repo: Option<Arc<dyn TraceDlqRepository>>,
    processing_timeout_ms: u64,
    max_result_size_bytes: usize,
) {
    // Restore W3C trace context from the originating request so this span
    // appears as a child in the caller's distributed trace.
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
        let _ = tracing::Span::current().set_parent(cx);
    }

    let trace_id = msg.trace_id;
    let channel = msg.channel;
    let start = Instant::now();

    // Capture payload/metadata for potential DLQ enqueue before consuming
    let payload_json_for_dlq = serde_json::to_string(&msg.payload).ok();
    let metadata_json_for_dlq = serde_json::to_string(&msg.metadata).ok();

    // Mark as running
    if let Err(e) = trace_repo
        .update_status(&trace_id, models::TRACE_STATUS_RUNNING, None)
        .await
    {
        tracing::error!(trace_id = %trace_id, error = %e, "Failed to update trace status to running");
        return;
    }

    // Build message
    let mut message = dataflow_rs::Message::from_value(&msg.payload);
    crate::engine::utils::merge_metadata(&mut message, &msg.metadata);
    crate::engine::utils::inject_rollout_bucket(&mut message);

    // Clone the inner Arc<Engine> and release the lock immediately
    let engine_ref = crate::engine::acquire_engine_read(&engine).await;
    let result = match tokio::time::timeout(
        Duration::from_millis(processing_timeout_ms),
        engine_ref.process_message_for_channel(&channel, &mut message),
    )
    .await
    {
        Ok(inner) => inner,
        Err(_) => {
            tracing::warn!(
                trace_id = %trace_id,
                channel = %channel,
                timeout_ms = processing_timeout_ms,
                "Async trace processing timed out"
            );
            Err(dataflow_rs::DataflowError::Timeout(format!(
                "Processing timed out after {}ms",
                processing_timeout_ms
            )))
        }
    };

    crate::engine::utils::remove_rollout_bucket(&mut message);

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let duration_ms = duration.as_secs_f64() * 1000.0;

    match result {
        Ok(()) => {
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration_secs);
            metrics::record_channel_execution(&channel);

            let result_json = match serde_json::to_string(&message) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!(trace_id = %trace_id, error = %e, "Failed to serialize trace result");
                    set_trace_status(
                        trace_repo.as_ref(),
                        &trace_id,
                        models::TRACE_STATUS_FAILED,
                        Some(&format!("Result serialization failed: {e}")),
                    )
                    .await;
                    return;
                }
            };

            // Enforce result size limit
            if max_result_size_bytes > 0 && result_json.len() > max_result_size_bytes {
                tracing::warn!(
                    trace_id = %trace_id,
                    result_bytes = result_json.len(),
                    limit_bytes = max_result_size_bytes,
                    "Trace result exceeds size limit"
                );
                metrics::record_error("result_size_exceeded");
                set_trace_status(
                    trace_repo.as_ref(),
                    &trace_id,
                    models::TRACE_STATUS_FAILED,
                    Some(&format!(
                        "Result size {} bytes exceeds limit of {} bytes",
                        result_json.len(),
                        max_result_size_bytes
                    )),
                )
                .await;
                return;
            }

            let mut result_saved = false;
            for attempt in 0..3 {
                match trace_repo
                    .set_result(&trace_id, &result_json, duration_ms)
                    .await
                {
                    Ok(_) => {
                        result_saved = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            trace_id = %trace_id, error = %e, attempt = attempt + 1,
                            "Failed to save trace result, retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
                    }
                }
            }

            if result_saved {
                set_trace_status(
                    trace_repo.as_ref(),
                    &trace_id,
                    models::TRACE_STATUS_COMPLETED,
                    None,
                )
                .await;
            } else {
                tracing::error!(trace_id = %trace_id, "Failed to save trace result after 3 attempts, marking as failed");
                set_trace_status(
                    trace_repo.as_ref(),
                    &trace_id,
                    models::TRACE_STATUS_FAILED,
                    Some("Result persistence failed after retries"),
                )
                .await;
            }
        }
        Err(e) => {
            metrics::record_message(&channel, "error");
            metrics::record_error("engine");

            let error_str = e.to_string();
            set_trace_status(
                trace_repo.as_ref(),
                &trace_id,
                models::TRACE_STATUS_FAILED,
                Some(&error_str),
            )
            .await;

            // Enqueue to DLQ for retry
            if let Some(ref dlq) = dlq_repo
                && let Some(ref payload) = payload_json_for_dlq
            {
                let metadata = metadata_json_for_dlq.as_deref().unwrap_or("{}");
                if let Err(dlq_err) = dlq
                    .enqueue(&trace_id, &channel, payload, metadata, &error_str, 5)
                    .await
                {
                    tracing::error!(
                        trace_id = %trace_id,
                        error = %dlq_err,
                        "Failed to enqueue failed trace to DLQ"
                    );
                } else {
                    tracing::info!(trace_id = %trace_id, "Failed trace enqueued to DLQ for retry");
                }
            }
        }
    }
}
