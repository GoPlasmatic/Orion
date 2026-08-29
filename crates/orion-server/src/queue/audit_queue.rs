//! Bounded, drained writer for admin audit-log rows (O7).
//!
//! Admin mutations must not wait on an audit INSERT, so the write is
//! asynchronous. It used to be a bare `tokio::spawn` per event with a `warn!`
//! on failure, which had two consequences an audit trail cannot have:
//!
//! * **Unbounded.** One task per mutation, each holding a DB connection. A
//!   bulk import of 1000 items spawned 1000 writers against a pool of 50.
//! * **Not drained.** Nothing awaited those tasks, so a mutation accepted
//!   moments before SIGTERM was answered `200` and then never recorded — the
//!   process exited with the write still in flight. The last thing an operator
//!   did before a rolling restart is exactly the row an investigation wants.
//!
//! This module replaces both with one bounded queue and one writer task that
//! is drained on shutdown. The drain is itself bounded (`audit.drain_timeout_secs`):
//! a database that has stopped accepting writes must not hold the process
//! open, so the drain gives up and says how many rows it abandoned.

use std::sync::Arc;
use std::time::Duration;

use super::bounded::{BoundedWorker, DrainHandle, DrainOutcome, DrainWitness, Rejected};
use crate::config::AuditConfig;
use crate::storage::repositories::audit_logs::AuditLogRepository;

/// One admin action, owned and ready to insert.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// The actor: a derived per-key id (see
    /// [`crate::server::admin_auth::AdminPrincipal`]) or `"anonymous"`.
    pub principal: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    /// JSON request context — `request_id`, `client_ip`, `user_agent`.
    pub details: Option<String>,
}

/// Producer handle held by `AppState`. Cheap to clone; every clone must be
/// dropped before the writer can finish draining.
#[derive(Clone)]
pub struct AuditQueue {
    queue: BoundedWorker<AuditEvent>,
}

impl AuditQueue {
    /// Enqueue an event without blocking the caller.
    ///
    /// A full queue means the writer cannot keep up — almost always a stalled
    /// database. Dropping is the only alternative to blocking an admin
    /// response behind that stall, so the drop is counted and logged at
    /// `error` rather than swallowed: `orion_audit_events_dropped_total` going
    /// non-zero means the audit trail has a hole in it.
    ///
    /// The reservation ordering that keeps the depth counter sound lives in
    /// `BoundedWorker::try_submit`; this method only decides what a refusal
    /// means for an audit trail.
    pub fn submit(&self, event: AuditEvent) {
        match self.queue.try_submit(event) {
            Ok(()) => {}
            Err(Rejected::Full(event)) => {
                crate::metrics::record_audit_event_dropped("queue_full");
                tracing::error!(
                    action = %event.action,
                    resource_type = %event.resource_type,
                    resource_id = %event.resource_id,
                    "Audit queue is full — this admin action was NOT recorded. \
                     Raise audit.max_pending or investigate why audit writes are stalled"
                );
            }
            Err(Rejected::Closed(event)) => {
                // Only reachable after the writer has exited, i.e. during
                // shutdown. Not an operator-actionable condition on its own.
                crate::metrics::record_audit_event_dropped("writer_stopped");
                tracing::warn!(
                    action = %event.action,
                    resource_type = %event.resource_type,
                    "Audit writer has stopped; this admin action was not recorded"
                );
            }
        }
    }

    /// Events accepted but not yet written — the value published as
    /// `orion_audit_queue_depth`.
    ///
    /// "Not yet written", not "not yet dequeued": the writer holds each event's
    /// lease across its INSERT, so this reaching zero is what lets the drain
    /// stop early (see `DrainWitness::QueueEmpty`).
    pub fn depth(&self) -> usize {
        self.queue.depth()
    }
}

/// Shutdown handle for the writer task.
pub struct AuditWriterHandle {
    join: tokio::task::JoinHandle<()>,
    drain: DrainHandle<AuditEvent>,
    drain_timeout: Duration,
}

impl AuditWriterHandle {
    /// Wait for the queue to drain, bounded by `audit.drain_timeout_secs`.
    ///
    /// Two ways to be finished, whichever comes first:
    ///
    /// * The writer task exits — which happens once every [`AuditQueue`] clone
    ///   is dropped and the buffer is empty. `main.rs` drops `AppState`
    ///   immediately before this call, so that is the normal path.
    /// * The queue depth reaches zero. This is the condition that actually
    ///   matters, and waiting on it as well means a background task still
    ///   holding an `AppState` clone the runtime has not finished dropping
    ///   (the cluster epoch watcher, just aborted) cannot stall shutdown for
    ///   the whole timeout over a queue that is already empty.
    ///
    /// Truncation is reported, never silent: an audit trail that lost rows has
    /// to say so, and the count is the number an investigator will not find.
    pub async fn shutdown(self) {
        let queued = self.drain.depth();
        if queued > 0 {
            tracing::info!(pending = queued, "Draining audit-log queue...");
        }

        match self
            .drain
            .drain(
                vec![self.join],
                DrainWitness::QueueEmpty,
                self.drain_timeout,
            )
            .await
        {
            DrainOutcome::Drained => {}
            DrainOutcome::WorkerPanicked => {
                tracing::error!("Audit writer task panicked")
            }
            DrainOutcome::TimedOut { lost } => {
                crate::metrics::record_audit_events_dropped("drain_timeout", lost as u64);
                tracing::error!(
                    lost,
                    drain_timeout_secs = self.drain_timeout.as_secs(),
                    "Audit-log drain timed out — these admin actions were NOT recorded. \
                     Raise audit.drain_timeout_secs or investigate the database"
                );
            }
        }
    }
}

/// Start the audit writer and return its producer handle.
///
/// One task, not a pool: audit volume is admin-mutation volume, and a single
/// in-order writer keeps the rows in the order the actions happened.
pub fn start(
    tasks: &crate::runtime::TaskRegistry,
    config: &AuditConfig,
    repo: Arc<dyn AuditLogRepository>,
) -> (AuditQueue, AuditWriterHandle) {
    let (queue, mut receivers) = BoundedWorker::<AuditEvent>::new(
        1,
        config.max_pending,
        crate::metrics::set_audit_queue_depth,
    );
    let mut rx = receivers.pop().expect("one shard was requested");
    let drain = queue.drain_handle();

    // Required: a dead writer means every admin mutation from here on is
    // unrecorded, which is the one failure an audit trail exists to prevent.
    // The join stays here because the drain is ordered — see `TaskHandles`.
    let guard = tasks.guard("audit_writer", crate::runtime::Criticality::Required);
    let join = tokio::spawn(guard.run(async move {
        // `recv_leased` yields `None` only once every sender is dropped *and*
        // the buffer is empty — so this loop is the drain. Each event's lease
        // is held across the INSERT and released when `event` goes out of
        // scope, which is what makes a depth of zero mean "written", not
        // merely "dequeued".
        while let Some(event) = rx.recv_leased().await {
            if let Err(e) = repo
                .insert(
                    &event.principal,
                    &event.action,
                    &event.resource_type,
                    &event.resource_id,
                    event.details.as_deref(),
                )
                .await
            {
                crate::metrics::record_audit_event_dropped("write_failed");
                tracing::error!(
                    error = %e,
                    action = %event.action,
                    resource_type = %event.resource_type,
                    resource_id = %event.resource_id,
                    "Failed to persist audit log entry"
                );
            }
        }
        crate::metrics::set_audit_queue_depth(0.0);
    }));

    (
        AuditQueue { queue },
        AuditWriterHandle {
            join,
            drain,
            drain_timeout: Duration::from_secs(config.drain_timeout_secs),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::OrionError;

    /// Records what it is asked to insert. Can be made to hang so the drain
    /// timeout is reachable, or to fail so the `write_failed` arm is.
    struct RecordingRepo {
        rows: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        block: Option<Duration>,
        fail: bool,
    }

    impl RecordingRepo {
        fn new(rows: Arc<std::sync::Mutex<Vec<(String, String)>>>) -> Self {
            Self {
                rows,
                block: None,
                fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl AuditLogRepository for RecordingRepo {
        async fn insert(
            &self,
            principal: &str,
            action: &str,
            _resource_type: &str,
            _resource_id: &str,
            _details: Option<&str>,
        ) -> Result<(), OrionError> {
            if let Some(d) = self.block {
                tokio::time::sleep(d).await;
            }
            if self.fail {
                return Err(OrionError::internal("audit insert failed".to_string()));
            }
            self.rows
                .lock()
                .expect("test mutex")
                .push((principal.to_string(), action.to_string()));
            Ok(())
        }

        async fn list_paginated(
            &self,
            _filter: &crate::storage::repositories::audit_logs::AuditLogFilter,
        ) -> Result<
            crate::storage::repositories::helpers::PaginatedResult<
                crate::storage::models::AuditLogEntry,
            >,
            OrionError,
        > {
            unimplemented!("not exercised")
        }

        async fn delete_older_than(&self, _days: u64) -> Result<u64, OrionError> {
            unimplemented!("not exercised")
        }
    }

    fn event(action: &str) -> AuditEvent {
        AuditEvent {
            principal: "key-0123456789abcdef".to_string(),
            action: action.to_string(),
            resource_type: "workflow".to_string(),
            resource_id: "wf-1".to_string(),
            details: None,
        }
    }

    /// The defect O7 names: a mutation accepted just before SIGTERM must still
    /// reach the database. Reverting the drain leaves this queue unread.
    #[tokio::test]
    async fn shutdown_drains_events_submitted_at_the_last_moment() {
        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let repo = Arc::new(RecordingRepo {
            block: Some(Duration::from_millis(20)),
            ..RecordingRepo::new(rows.clone())
        });
        let (queue, handle) = start(
            &crate::runtime::TaskRegistry::new(),
            &AuditConfig::default(),
            repo,
        );
        for i in 0..5 {
            queue.submit(event(&format!("action-{i}")));
        }
        // Exactly the production sequence: drop every producer, then drain.
        drop(queue);
        handle.shutdown().await;
        let written = rows.lock().expect("test mutex").len();
        assert_eq!(
            written, 5,
            "every event enqueued before shutdown must be written"
        );
    }

    /// A stalled database must not hold the process open — the drain gives up
    /// and the loss is counted rather than hidden.
    #[tokio::test]
    async fn drain_is_bounded_when_writes_hang() {
        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let repo = Arc::new(RecordingRepo {
            block: Some(Duration::from_secs(3600)),
            ..RecordingRepo::new(rows.clone())
        });
        let config = AuditConfig {
            drain_timeout_secs: 1,
            ..AuditConfig::default()
        };
        let (queue, handle) = start(&crate::runtime::TaskRegistry::new(), &config, repo);
        queue.submit(event("stuck"));
        drop(queue);
        let started = tokio::time::Instant::now();
        handle.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the drain must be bounded by drain_timeout_secs, not by the database"
        );
        assert!(rows.lock().expect("test mutex").is_empty());
    }

    /// An empty queue must finish the drain immediately even if some other
    /// holder of the producer has not been dropped yet — the cluster epoch
    /// watcher holds an `AppState` clone and is only just aborted when this
    /// runs. Waiting on the channel closing alone would burn the whole
    /// timeout and then report a loss of zero rows.
    #[tokio::test]
    async fn a_lingering_producer_does_not_stall_an_empty_drain() {
        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let repo = Arc::new(RecordingRepo::new(rows.clone()));
        let config = AuditConfig {
            drain_timeout_secs: 30,
            ..AuditConfig::default()
        };
        let (queue, handle) = start(&crate::runtime::TaskRegistry::new(), &config, repo);
        queue.submit(event("recorded"));
        // Deliberately kept alive across the shutdown.
        let _stray = queue.clone();
        drop(queue);

        let started = tokio::time::Instant::now();
        handle.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an empty queue must end the drain, not the 30s timeout"
        );
        assert_eq!(rows.lock().expect("test mutex").len(), 1);
    }

    /// Overflow is bounded and visible: submissions past `max_pending` are
    /// dropped rather than queued without limit or blocking the caller.
    #[tokio::test]
    async fn queue_is_bounded_and_overflow_does_not_block() {
        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let repo = Arc::new(RecordingRepo {
            block: Some(Duration::from_secs(3600)),
            ..RecordingRepo::new(rows.clone())
        });
        let config = AuditConfig {
            max_pending: 2,
            drain_timeout_secs: 1,
            ..AuditConfig::default()
        };
        let (queue, handle) = start(&crate::runtime::TaskRegistry::new(), &config, repo);
        // `submit` must return promptly even well past capacity.
        for i in 0..50 {
            queue.submit(event(&format!("a{i}")));
        }
        assert!(
            queue.depth() <= 3,
            "queue depth must stay bounded by max_pending (+1 in the writer), got {}",
            queue.depth()
        );
        drop(queue);
        handle.shutdown().await;
    }

    /// The depth counter has to be a sound witness of "nothing is buffered":
    /// [`AuditWriterHandle::shutdown`] aborts the writer the moment it reads
    /// zero, and the same value is published as `orion_audit_queue_depth` and
    /// added to the drop counter on a timeout.
    ///
    /// Counting *after* the send let a fast writer's `fetch_sub` run at zero
    /// and wrap the `AtomicUsize` to `usize::MAX`. This drives many producers
    /// against a writer that never blocks, so the interleaving is reachable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn depth_is_a_sound_witness_under_a_fast_writer() {
        const PRODUCERS: usize = 8;
        const PER_PRODUCER: usize = 250;

        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let repo = Arc::new(RecordingRepo::new(rows.clone()));
        let config = AuditConfig {
            max_pending: 4096,
            drain_timeout_secs: 30,
            ..AuditConfig::default()
        };
        let (queue, handle) = start(&crate::runtime::TaskRegistry::new(), &config, repo);

        let mut producers = tokio::task::JoinSet::new();
        for p in 0..PRODUCERS {
            let queue = queue.clone();
            producers.spawn(async move {
                for i in 0..PER_PRODUCER {
                    queue.submit(event(&format!("a{p}-{i}")));
                    tokio::task::yield_now().await;
                }
            });
        }
        while let Some(joined) = producers.join_next().await {
            joined.expect("producer task");
        }

        assert!(
            queue.depth() <= PRODUCERS * PER_PRODUCER,
            "depth wrapped: {} submissions cannot leave a depth of {}",
            PRODUCERS * PER_PRODUCER,
            queue.depth()
        );

        drop(queue);
        handle.shutdown().await;
        assert_eq!(
            rows.lock().expect("test mutex").len(),
            PRODUCERS * PER_PRODUCER,
            "a zero reading must not end the drain while rows are still buffered"
        );
    }

    /// A failing INSERT is a hole in the audit trail, so it has to reach the
    /// counter the observability page tells operators to alert on. The `Ok`-only
    /// mock left this arm — and the drop metric on it — entirely unexercised.
    #[test]
    fn a_failed_insert_is_counted_as_a_drop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // The local recorder is thread-local and the current-thread runtime
        // drives the writer task on this very thread, so its `record_*` calls
        // land in this exposition.
        let exposition = crate::metrics::render_local(|| {
            rt.block_on(async {
                let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
                let repo = Arc::new(RecordingRepo {
                    fail: true,
                    ..RecordingRepo::new(rows.clone())
                });
                let (queue, handle) = start(
                    &crate::runtime::TaskRegistry::new(),
                    &AuditConfig::default(),
                    repo,
                );
                queue.submit(event("delete"));
                queue.submit(event("update"));
                drop(queue);
                handle.shutdown().await;
                assert!(
                    rows.lock().expect("test mutex").is_empty(),
                    "the mock refused both inserts"
                );
            });
        });
        assert!(
            exposition.contains(r#"orion_audit_events_dropped_total{reason="write_failed"} 2"#),
            "a failed audit INSERT must be counted, not just logged:\n{exposition}"
        );
    }

    /// Overflow reaches the same counter under its own reason, so an operator
    /// can tell "the writer fell behind" from "the database refused the row".
    #[test]
    fn overflow_is_counted_as_a_drop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let exposition = crate::metrics::render_local(|| {
            rt.block_on(async {
                let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
                let repo = Arc::new(RecordingRepo {
                    block: Some(Duration::from_secs(3600)),
                    ..RecordingRepo::new(rows.clone())
                });
                let config = AuditConfig {
                    max_pending: 2,
                    drain_timeout_secs: 1,
                    ..AuditConfig::default()
                };
                let (queue, handle) = start(&crate::runtime::TaskRegistry::new(), &config, repo);
                for i in 0..10 {
                    queue.submit(event(&format!("a{i}")));
                }
                drop(queue);
                handle.shutdown().await;
            });
        });
        assert!(
            exposition.contains(r#"orion_audit_events_dropped_total{reason="queue_full"} 8"#),
            "the 8 submissions past max_pending must be counted:\n{exposition}"
        );
    }
}
