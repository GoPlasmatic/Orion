use std::sync::Arc;
use std::time::Duration;

use crate::metrics;
use crate::storage::repositories::trace_dlq::TraceDlqRepository;
use crate::storage::repositories::traces::TraceRepository;

use super::{QueueMessage, TraceQueue};

/// Options for [`start_dlq_retry`].
pub struct DlqRetryOptions {
    pub poll_interval_secs: u64,
    /// Maximum entries claimed per tick (`queue.dlq_batch_size`).
    pub batch_size: i64,
    /// Per-row lease duration (`queue.dlq_lease_secs`) — an expired lease is
    /// re-claimable, so a crashed node's claims recover automatically.
    pub lease_secs: u64,
    /// Lease/claim identity: the cluster instance id.
    pub claimant: String,
    /// Cluster-mode single-flight gate; `None` on a single node. The gate is
    /// an efficiency measure (skip whole ticks); per-row claims are the
    /// correctness layer either way.
    pub lease_gate: Option<Arc<crate::cluster::JobLeaseGate>>,
}

/// Start a background task that polls the trace DLQ and re-submits entries
/// for processing via the trace queue.
///
/// Uses exponential backoff: delay = base_delay * 2^retry_count (1s, 2s, 4s, 8s, 16s, ...).
/// After `max_retries`, the entry is marked as exhausted and no longer retried.
pub fn start_dlq_retry(
    opts: DlqRetryOptions,
    dlq_repo: Arc<dyn TraceDlqRepository>,
    trace_queue: TraceQueue,
    trace_repo: Arc<dyn TraceRepository>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(opts.poll_interval_secs));
        // Skip the first immediate tick
        interval.tick().await;
        let job_lease_ttl = opts.poll_interval_secs + 30;

        loop {
            interval.tick().await;

            // Refresh the depth gauge before the single-flight gate: the count
            // is a cheap read and every node should report its own view of the
            // DLQ, not just whichever one wins the lease this tick (O2).
            match dlq_repo
                .count(&crate::storage::repositories::trace_dlq::TraceDlqFilter::default())
                .await
            {
                Ok(depth) => metrics::set_trace_dlq_depth(depth as f64),
                Err(e) => tracing::warn!(error = %e, "DLQ retry: failed to read DLQ depth"),
            }

            if let Some(ref gate) = opts.lease_gate
                && !gate.try_acquire("dlq_retry", job_lease_ttl).await
            {
                continue;
            }

            let entries = match dlq_repo
                .claim_pending(&opts.claimant, opts.batch_size, opts.lease_secs)
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "DLQ retry: failed to claim pending entries");
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
                        metrics::record_trace_dlq_retry("exhausted");
                        if let Err(e) = dlq_repo.mark_exhausted(&entry.id).await {
                            tracing::error!(
                                dlq_id = %entry.id,
                                error = %e,
                                "DLQ retry: failed to mark corrupt entry exhausted — \
                                 it will be re-claimed when its lease expires"
                            );
                        }
                        continue;
                    }
                };

                let metadata: serde_json::Value =
                    serde_json::from_str(&entry.metadata_json).unwrap_or_default();

                // Create a new pending trace for the retry. DLQ rows only
                // carry the channel *name*; resolve the ID from the registry
                // (NULL when the channel is no longer active — honest).
                let channel_id = channel_registry
                    .get_by_name(&entry.channel)
                    .await
                    .map(|c| c.channel.channel_id.clone());
                let new_trace = match trace_repo
                    .create_pending(
                        &entry.channel,
                        channel_id.as_deref(),
                        "async",
                        Some(&entry.payload_json),
                        // Operator-plane row: the original submitter's token
                        // died with the failed trace; reads follow the admin
                        // trust model (R12).
                        None,
                    )
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            dlq_id = %entry.id,
                            error = %e,
                            "DLQ retry: failed to create pending trace"
                        );
                        metrics::record_trace_dlq_retry("failed");
                        continue;
                    }
                };

                // Submit to the trace queue. The row is deleted below once the
                // submit succeeds, so the attempt this resubmission represents
                // travels with the message — a repeat failure re-enters the
                // DLQ at `retry_count + 1` rather than 0 (Q3).
                let submit_result = trace_queue
                    .submit_dlq_retry(
                        QueueMessage {
                            trace_id: new_trace.id.clone(),
                            channel: entry.channel.clone(),
                            payload,
                            metadata,
                            trace_headers: std::collections::HashMap::new(),
                            profile_requested: false,
                            backpressure_permit: None,
                        },
                        entry.retry_count + 1,
                    )
                    .await;

                match submit_result {
                    Ok(()) => {
                        // Successfully re-submitted — remove from DLQ
                        metrics::record_trace_dlq_retry("retried");
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
                        // Both writes clear the claim lease. Swallowing their
                        // errors is what let D1's postgres type mismatch stay
                        // invisible while entries were re-claimed forever.
                        if new_retry_count >= entry.max_retries {
                            metrics::record_error("dlq_exhausted");
                            metrics::record_trace_dlq_retry("exhausted");
                            if let Err(e) = dlq_repo.mark_exhausted(&entry.id).await {
                                tracing::error!(
                                    dlq_id = %entry.id,
                                    error = %e,
                                    "DLQ retry: failed to retire exhausted entry — \
                                     it will be re-claimed when its lease expires"
                                );
                            } else {
                                tracing::warn!(
                                    dlq_id = %entry.id,
                                    "DLQ retry: max retries exhausted, giving up"
                                );
                            }
                        } else {
                            metrics::record_trace_dlq_retry("failed");
                            // Exponential backoff: 1s, 2s, 4s, 8s, 16s, …
                            // capped at 2^16s (~18h). Unclamped, a large
                            // dlq_max_retries overflows the shift (Q4):
                            // panic in debug, nonsense in release — either
                            // way the retry task dies for the process
                            // lifetime. The validator bounds the config,
                            // this bounds the arithmetic.
                            let delay_secs = 1i64 << new_retry_count.min(16);
                            let next_retry = chrono::Utc::now()
                                .naive_utc()
                                .checked_add_signed(chrono::Duration::seconds(delay_secs))
                                .unwrap_or(chrono::Utc::now().naive_utc());
                            if let Err(e) = dlq_repo.record_retry(&entry.id, next_retry).await {
                                tracing::error!(
                                    dlq_id = %entry.id,
                                    error = %e,
                                    "DLQ retry: failed to record backoff — entry keeps its \
                                     old next_retry_at and will be re-claimed on lease expiry"
                                );
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::OrionError;
    use crate::storage::models::{Trace, TraceDlqEntry};
    use crate::storage::repositories::trace_dlq::TraceDlqRepository;
    use crate::storage::repositories::traces::TraceRepository;
    use crate::storage::repositories::helpers::PaginatedResult;
    use async_trait::async_trait;
    use std::sync::Mutex;

    // ----------------------------------------------------------------
    // Mock TraceDlqRepository
    // ----------------------------------------------------------------

    #[derive(Default)]
    struct MockDlqState {
        entries_to_return: Vec<TraceDlqEntry>,
        removed_ids: Vec<String>,
        exhausted_ids: Vec<String>,
        retried: Vec<(String, String)>, // (id, next_retry_at)
        count_calls: usize,
    }

    struct MockDlqRepo {
        state: Mutex<MockDlqState>,
    }

    impl MockDlqRepo {
        fn new(entries: Vec<TraceDlqEntry>) -> Self {
            Self {
                state: Mutex::new(MockDlqState {
                    entries_to_return: entries,
                    ..Default::default()
                }),
            }
        }
    }

    #[async_trait]
    impl TraceDlqRepository for MockDlqRepo {
        async fn enqueue(
            &self,
            _trace_id: &str,
            _channel: &str,
            _payload_json: &str,
            _metadata_json: &str,
            _error_message: &str,
            _retry_count: i64,
            _max_retries: i64,
        ) -> Result<TraceDlqEntry, OrionError> {
            unimplemented!("not needed for retry tests")
        }

        async fn claim_pending(
            &self,
            _claimant: &str,
            _limit: i64,
            _lease_secs: u64,
        ) -> Result<Vec<TraceDlqEntry>, OrionError> {
            let mut state = self.state.lock().expect("test");
            // Return entries once, then empty (simulates consumption)
            let entries = std::mem::take(&mut state.entries_to_return);
            Ok(entries)
        }

        async fn record_retry(
            &self,
            id: &str,
            next_retry_at: chrono::NaiveDateTime,
        ) -> Result<(), OrionError> {
            self.state
                .lock()
                .expect("test")
                .retried
                .push((id.to_string(), next_retry_at.to_string()));
            Ok(())
        }

        async fn remove(&self, id: &str) -> Result<(), OrionError> {
            self.state
                .lock()
                .expect("test")
                .removed_ids
                .push(id.to_string());
            Ok(())
        }

        async fn mark_exhausted(&self, id: &str) -> Result<(), OrionError> {
            self.state
                .lock()
                .expect("test")
                .exhausted_ids
                .push(id.to_string());
            Ok(())
        }

        async fn list_paginated(
            &self,
            _filter: &crate::storage::repositories::trace_dlq::TraceDlqFilter,
        ) -> Result<
            PaginatedResult<crate::storage::repositories::trace_dlq::TraceDlqSummary>,
            OrionError,
        > {
            unimplemented!("not needed for retry tests")
        }

        async fn count(
            &self,
            _filter: &crate::storage::repositories::trace_dlq::TraceDlqFilter,
        ) -> Result<i64, OrionError> {
            let mut state = self.state.lock().expect("test");
            state.count_calls += 1;
            Ok(state.entries_to_return.len() as i64)
        }

        async fn get_by_id(&self, _id: &str) -> Result<TraceDlqEntry, OrionError> {
            unimplemented!("not needed for retry tests")
        }

        async fn requeue(&self, _id: &str) -> Result<TraceDlqEntry, OrionError> {
            unimplemented!("not needed for retry tests")
        }

        async fn purge_exhausted(&self, _older_than_hours: u64) -> Result<u64, OrionError> {
            unimplemented!("not needed for retry tests")
        }
    }

    // ----------------------------------------------------------------
    // Mock TraceRepository (only create_pending needed)
    // ----------------------------------------------------------------

    struct MockTraceRepo;

    fn make_trace(id: &str, channel: &str) -> Trace {
        let now = chrono::Utc::now().naive_utc();
        Trace {
            id: id.to_string(),
            channel: channel.to_string(),
            channel_id: None,
            mode: "async".to_string(),
            status: "pending".to_string(),
            input_json: None,
            result_json: None,
            error_message: None,
            duration_ms: None,
            started_at: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            task_trace_json: None,
            access_token_hash: None,
        }
    }

    #[async_trait]
    impl TraceRepository for MockTraceRepo {
        async fn create_pending(
            &self,
            channel: &str,
            _channel_id: Option<&str>,
            _mode: &str,
            _input_json: Option<&str>,
            _access_token_hash: Option<&str>,
        ) -> Result<Trace, OrionError> {
            Ok(make_trace(&uuid::Uuid::new_v4().to_string(), channel))
        }
        async fn get_by_id(&self, _id: &str) -> Result<Trace, OrionError> {
            unimplemented!()
        }
        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _error_message: Option<&str>,
        ) -> Result<Trace, OrionError> {
            unimplemented!()
        }
        async fn set_result(
            &self,
            _id: &str,
            _result_json: &str,
            _duration_ms: f64,
            _task_trace_json: Option<&str>,
        ) -> Result<(), OrionError> {
            unimplemented!()
        }
        async fn store_completed(
            &self,
            _channel: &str,
            _channel_id: Option<&str>,
            _mode: &str,
            _input_json: Option<&str>,
            _result_json: &str,
            _duration_ms: f64,
            _task_trace_json: Option<&str>,
        ) -> Result<String, OrionError> {
            unimplemented!()
        }
        async fn list_paginated(
            &self,
            _filter: &crate::storage::repositories::traces::TraceFilter,
        ) -> Result<PaginatedResult<Trace>, OrionError> {
            unimplemented!()
        }
        async fn delete_older_than(&self, _hours: u64) -> Result<u64, OrionError> {
            unimplemented!()
        }
    }

    // ----------------------------------------------------------------
    // Helpers
    // ----------------------------------------------------------------

    fn make_dlq_entry(
        id: &str,
        payload_json: &str,
        retry_count: i64,
        max_retries: i64,
    ) -> TraceDlqEntry {
        let now = chrono::Utc::now().naive_utc();
        TraceDlqEntry {
            id: id.to_string(),
            trace_id: format!("trace-{}", id),
            channel: "test-channel".to_string(),
            payload_json: payload_json.to_string(),
            metadata_json: "{}".to_string(),
            error_message: "test error".to_string(),
            retry_count,
            max_retries,
            next_retry_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_test_queue(
        buffer_size: usize,
    ) -> (
        TraceQueue,
        tokio::sync::mpsc::Receiver<crate::queue::QueuedItem>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::queue::QueuedItem>(buffer_size);
        let queue = TraceQueue::new_for_test(tx);
        (queue, rx)
    }

    fn make_closed_queue() -> TraceQueue {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::queue::QueuedItem>(1);
        drop(rx); // receiver dropped → send will fail
        TraceQueue::new_for_test(tx)
    }

    // ----------------------------------------------------------------
    // Helpers for advancing time
    // ----------------------------------------------------------------

    /// Advance time and yield repeatedly to let spawned tasks run.
    async fn advance_and_yield(duration: Duration) {
        tokio::time::advance(duration).await;
        // Yield multiple times to allow the spawned task's interval to fire
        // and all subsequent async operations to complete
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    // ----------------------------------------------------------------
    // Tests
    // ----------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn test_successful_retry_removes_from_dlq() {
        let entry = make_dlq_entry("e1", r#"{"key":"value"}"#, 0, 5);
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![entry]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let (queue, mut rx) = make_test_queue(10);

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        // Advance past the first skipped tick
        advance_and_yield(Duration::from_secs(1)).await;
        // Advance past the second tick (actual poll)
        advance_and_yield(Duration::from_secs(1)).await;

        // Drain the submitted message to unblock the sender
        let _ = rx.try_recv();
        advance_and_yield(Duration::from_millis(100)).await;

        handle.abort();

        let state = dlq_repo.state.lock().expect("test");
        assert!(
            state.removed_ids.contains(&"e1".to_string()),
            "entry should be removed after successful retry, removed: {:?}",
            state.removed_ids
        );
    }

    /// O2: `trace_dlq_depth` is refreshed from this loop, so the count has to
    /// run on every tick — including ticks that claim nothing.
    #[tokio::test(start_paused = true)]
    async fn test_depth_is_refreshed_every_tick() {
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let (queue, _rx) = make_test_queue(10);

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        handle.abort();

        assert!(
            dlq_repo.state.lock().expect("test").count_calls >= 2,
            "the depth gauge must be refreshed on every poll tick"
        );
    }

    /// Q3 regression: the resubmitted message must carry the attempt it
    /// represents. Without it the worker re-enqueues at 0 on failure and the
    /// entry never converges on `max_retries`.
    #[tokio::test(start_paused = true)]
    async fn test_resubmission_carries_originating_retry_count() {
        let entry = make_dlq_entry("e5", r#"{"ok":true}"#, 2, 5);
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![entry]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let (queue, mut rx) = make_test_queue(10);

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;

        let item = rx.try_recv().expect("entry should have been resubmitted");
        handle.abort();

        assert_eq!(
            item.dlq_retry_count, 3,
            "resubmission must carry retry_count + 1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_corrupt_payload_marks_exhausted() {
        let entry = make_dlq_entry("e2", "not valid json!!!", 0, 5);
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![entry]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let (queue, _rx) = make_test_queue(10);

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_millis(100)).await;

        handle.abort();

        let state = dlq_repo.state.lock().expect("test");
        assert!(
            state.exhausted_ids.contains(&"e2".to_string()),
            "corrupt payload should mark entry as exhausted, exhausted: {:?}",
            state.exhausted_ids
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_max_retries_exhausted_marks_entry() {
        // Entry at max_retries - 1, so one more failure should exhaust it
        let entry = make_dlq_entry("e3", r#"{"ok":true}"#, 4, 5);
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![entry]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let queue = make_closed_queue();

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_millis(100)).await;

        handle.abort();

        let state = dlq_repo.state.lock().expect("test");
        assert!(
            state.exhausted_ids.contains(&"e3".to_string()),
            "entry at max retries should be marked exhausted, exhausted: {:?}",
            state.exhausted_ids
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_queue_full_records_retry_with_backoff() {
        // Entry at retry_count=1, max_retries=5 — plenty of room for backoff
        let entry = make_dlq_entry("e4", r#"{"ok":true}"#, 1, 5);
        let dlq_repo = Arc::new(MockDlqRepo::new(vec![entry]));
        let trace_repo: Arc<dyn TraceRepository> = Arc::new(MockTraceRepo);
        let queue = make_closed_queue();

        let handle = start_dlq_retry(
            DlqRetryOptions {
                poll_interval_secs: 1,
                batch_size: 20,
                lease_secs: 60,
                claimant: "test-node".to_string(),
                lease_gate: None,
            },
            dlq_repo.clone(),
            queue,
            trace_repo,
            Arc::new(crate::channel::ChannelRegistry::new()),
        );

        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_secs(1)).await;
        advance_and_yield(Duration::from_millis(100)).await;

        handle.abort();

        let state = dlq_repo.state.lock().expect("test");
        assert_eq!(
            state.retried.len(),
            1,
            "should have recorded one retry, got: {:?}",
            state.retried
        );
        let (id, _next_retry_at) = &state.retried[0];
        assert_eq!(id, "e4");
    }
}
