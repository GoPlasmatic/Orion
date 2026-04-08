use std::sync::Arc;
use std::time::Duration;

use crate::metrics;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

use super::{QueueMessage, TraceQueue};

/// Start a background task that polls the trace DLQ and re-submits entries
/// for processing via the trace queue.
///
/// Uses exponential backoff: delay = base_delay * 2^retry_count (1s, 2s, 4s, 8s, 16s, ...).
/// After `max_retries`, the entry is marked as exhausted and no longer retried.
pub fn start_dlq_retry(
    poll_interval_secs: u64,
    dlq_repo: Arc<dyn TraceDlqRepository>,
    trace_queue: TraceQueue,
    trace_repo: Arc<dyn TraceRepository>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
        // Skip the first immediate tick
        interval.tick().await;

        loop {
            interval.tick().await;

            let entries = match dlq_repo.list_pending(20).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "DLQ retry: failed to fetch pending entries");
                    continue;
                }
            };

            for entry in entries {
                let payload: serde_json::Value = match serde_json::from_str(&entry.payload_json) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: corrupt payload, marking exhausted"
                        );
                        let _ = dlq_repo.mark_exhausted(&entry.id).await;
                        continue;
                    }
                };

                let metadata: serde_json::Value =
                    serde_json::from_str(&entry.metadata_json).unwrap_or_default();

                // Create a new pending trace for the retry
                let new_trace = match trace_repo
                    .create_pending(&entry.channel, "async", Some(&entry.payload_json))
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: failed to create pending trace"
                        );
                        continue;
                    }
                };

                // Submit to the trace queue
                let submit_result = trace_queue
                    .submit(QueueMessage {
                        trace_id: new_trace.id.clone(),
                        channel: entry.channel.clone(),
                        payload,
                        metadata,
                        trace_headers: std::collections::HashMap::new(),
                    })
                    .await;

                match submit_result {
                    Ok(()) => {
                        // Successfully re-submitted — remove from DLQ
                        if let Err(e) = dlq_repo.remove(&entry.id).await {
                            tracing::error!(
                                dlq_id = %entry.id,
                                error = %e,
                                "DLQ retry: failed to remove entry after successful resubmit"
                            );
                        } else {
                            tracing::info!(
                                dlq_id = %entry.id,
                                retry = entry.retry_count + 1,
                                new_trace_id = %new_trace.id,
                                "DLQ retry: trace re-submitted successfully"
                            );
                        }
                    }
                    Err(e) => {
                        // Queue is full or closed — bump retry count with backoff
                        tracing::warn!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: failed to resubmit, scheduling next retry"
                        );
                        let new_retry_count = entry.retry_count + 1;
                        if new_retry_count >= entry.max_retries {
                            metrics::record_error("dlq_exhausted");
                            let _ = dlq_repo.mark_exhausted(&entry.id).await;
                            tracing::warn!(
                                dlq_id = %entry.id,
                                "DLQ retry: max retries exhausted, giving up"
                            );
                        } else {
                            // Exponential backoff: 1s, 2s, 4s, 8s, 16s, ...
                            let delay_secs = 1i64 << new_retry_count;
                            let next_retry = chrono::Utc::now()
                                .naive_utc()
                                .checked_add_signed(chrono::Duration::seconds(delay_secs))
                                .unwrap_or(chrono::Utc::now().naive_utc())
                                .to_string();
                            let _ = dlq_repo.record_retry(&entry.id, &next_retry).await;
                        }
                    }
                }
            }
        }
    })
}
