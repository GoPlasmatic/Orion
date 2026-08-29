//! Kafka ingestion consumer.
//!
//! Delivery guarantee: **at-least-once**. A message's offset is committed
//! only after the message was processed successfully, recognised as a
//! duplicate by the channel's deduplication window (N16 — an earlier
//! delivery already did the work), or had its payload confirmed written to
//! the DLQ. On any other outcome (processing failure with the DLQ disabled,
//! a failed DLQ write, or a channel guard deferring the message because it
//! is over its rate limit or at capacity) the offset stays uncommitted and
//! the consumer retries the same message in place with capped exponential
//! backoff — which is also how a rate-limited channel throttles its topic
//! rather than discarding it. The in-place window is bounded to stay
//! safely below `max.poll.interval.ms` — 80% of it, 240s against
//! librdkafka's 300s default — because retrying blocks polling, and a
//! consumer that stops polling for that long is evicted from the group.
//! On expiry the partition is rewound to the message's offset and the loop
//! returns to polling, so the same message is redelivered and rebalance
//! callbacks keep firing (rdkafka dispatches them only from the polling
//! thread).
//!
//! Processing is **strictly sequential** — one message at a time per
//! consumer, across all of its assigned partitions. This is load-bearing
//! for the guarantee above: committing an offset implicitly commits every
//! earlier offset on that partition, so any in-consumer concurrency would
//! let a fast later message commit past a failed earlier one and lose it
//! (K4 — the former `kafka.max_inflight` knob advertised concurrency that
//! never existed and was removed). Scale throughput by running more
//! instances in the same consumer group, which spreads partitions across
//! them; a restart redelivers from the failed message. Enable `kafka.dlq`
//! to avoid head-of-line blocking on poison messages (e.g. payloads that
//! will never parse).

mod context;
mod dlq;
mod lag;
mod process;

use dataflow_rs::datalogic_rs;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::watch;

use crate::config::KafkaIngestConfig;
use crate::errors::OrionError;
use crate::kafka::producer::KafkaProducer;

use context::{KafkaConsumerContext, RebalanceState};
use lag::poll_consumer_lag;
use process::process_until_committed;
pub(crate) use process::{INITIAL_RETRY_BACKOFF_MS, next_backoff_ms};

/// Bundled context for the Kafka consume loop, grouping parameters that share
/// the same lifecycle and reducing positional argument count.
/// Everything a consumer needs from the running instance, beyond its own
/// `KafkaIngestConfig`.
///
/// A struct rather than positional parameters because half of these are
/// `Option`: `start_consumer(&cfg, engine, registry, datalogic, None, None,
/// None, None)` says nothing about which `None` is which, and the next
/// optional dependency would add a ninth argument to a list already past
/// clippy's limit.
pub struct ConsumerDeps {
    pub engine: Arc<crate::engine::EngineHandle>,
    pub channel_registry: Arc<crate::channel::ChannelRegistry>,
    pub datalogic: Arc<datalogic_rs::Engine>,
    /// `[vars]` as one JSON object, stamped into every ingested message's
    /// `metadata.vars` — the Kafka half of what `build_request_metadata` does
    /// for HTTP, so a workflow reads the same deployment values on either
    /// transport. `None` when the instance declares no vars.
    pub vars: Option<Arc<serde_json::Value>>,
    pub dlq_producer: Option<Arc<KafkaProducer>>,
    pub dlq_topic: Option<String>,
    /// Cluster mode: enables static group membership (`group.instance.id`)
    /// plus an explicit `session.timeout.ms`, so rolling restarts and
    /// reload-driven consumer restarts rejoin without a full group rebalance.
    /// `None` (single node) keeps dynamic membership.
    pub instance_id: Option<String>,
    /// Where a completed message's trace row goes. See `ConsumeLoopContext`.
    pub trace_repo: Arc<dyn crate::storage::repositories::traces::TraceSink>,
    pub persistence_queue: crate::queue::TracePersistenceQueue,
    /// `trace_queue.max_result_size_bytes` — the cap on a persisted result,
    /// applied here for the same reason the async path applies it.
    pub max_result_size_bytes: usize,
}

/// What one consume loop needs to dispatch a record.
///
/// This carries a trace repository and a persistence queue, which it did not
/// used to. A Kafka-ingested message shares admission
/// (`channel::guards::admit`) and dispatch (`engine::execute_admitted`) with
/// HTTP, but wrote no `traces` row — so it was invisible to
/// `/api/v1/data/traces` and to the trace DLQ, while the HTTP `/async` path was
/// fully traced. The reason recorded here was that closing it meant writing a
/// fifth copy of the trace-plan/serialize/route sequence in
/// `routes/data/sync.rs`, and that it would close "when that step becomes one
/// function". It is one function now — `queue::trace_record` — so this is that.
struct ConsumeLoopContext {
    consumer: Arc<StreamConsumer<KafkaConsumerContext>>,
    topic_map: HashMap<String, String>,
    engine: Arc<crate::engine::EngineHandle>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    datalogic: Arc<datalogic_rs::Engine>,
    vars: Option<Arc<serde_json::Value>>,
    dlq_producer: Option<Arc<KafkaProducer>>,
    dlq_topic: Option<String>,
    processing_timeout_ms: u64,
    lag_poll_interval_secs: u64,
    /// Where a completed Kafka message's trace row is written.
    ///
    /// The consumer shares admission and dispatch with HTTP but wrote no trace
    /// at all, so a Kafka-ingested message never appeared in
    /// `GET /api/v1/data/traces` and could not be retried from the DLQ. These
    /// three are what the shared `queue::trace_record` needs.
    trace_repo: Arc<dyn crate::storage::repositories::traces::TraceSink>,
    persistence_queue: crate::queue::TracePersistenceQueue,
    max_result_size_bytes: usize,
    /// Rebalance bookkeeping shared with the consumer context (K8). The
    /// revocation flag is dispatched only from `recv()` polls, so the retry
    /// loop can observe it between messages, never mid-retry — see
    /// `process::process_until_committed` for the dispatch semantics and
    /// the bounded-retry consequence.
    rebalance: Arc<RebalanceState>,
    /// In-place retry budget (K8): how long `process_until_committed` may
    /// stay away from the poll loop before rewinding the partition. Derived
    /// from `max.poll.interval.ms` by `process::in_place_retry_budget_ms`.
    retry_budget_ms: u64,
}

/// Grace added to `kafka.processing_timeout_ms` to get the shutdown join
/// deadline: enough for one in-flight dispatch to finish, plus the commit and
/// the loop's own exit, and no more.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// The join deadline for one consumer, derived from its own per-message
/// budget rather than picked as a constant: the loop can be inside a dispatch
/// when the signal arrives, and that dispatch is already bounded by
/// `kafka.processing_timeout_ms` (`process_until_committed` clamps a channel's
/// `timeout_ms` to it), so anything shorter would routinely report a healthy
/// consumer as wedged. `saturating_add` because `Duration`'s `+` panics on
/// overflow and `processing_timeout_ms` is operator input — unreachable at any
/// value a person would type, and not worth leaving to that.
fn shutdown_deadline(processing_timeout_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(processing_timeout_ms).saturating_add(SHUTDOWN_GRACE)
}

/// Handle for managing the Kafka consumer lifecycle.
pub struct ConsumerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<()>,
    consumer: Arc<StreamConsumer<KafkaConsumerContext>>,
    topics: HashSet<String>,
    rebalance: Arc<RebalanceState>,
    /// Deadline for [`Self::shutdown`]'s join.
    shutdown_timeout: std::time::Duration,
}

impl ConsumerHandle {
    /// Signal the consumer to shut down and wait for it to finish, bounded by
    /// `kafka.processing_timeout_ms` plus a fixed grace (`SHUTDOWN_GRACE`).
    ///
    /// The bound is what makes this callable from a request path. The loop
    /// checks the shutdown watch between polls and inside its retry backoff,
    /// so it normally exits within one dispatch — but rdkafka's `recv()` and
    /// an offset commit are both blocking calls into the C client, and a
    /// broker that stops answering makes them arbitrarily slow. Unbounded,
    /// that hangs SIGTERM handling *and* an engine reload, which now shuts a
    /// consumer down while holding `AppStateInner::reload_lock`.
    ///
    /// On timeout the task is left detached rather than aborted: it holds a
    /// `StreamConsumer` whose drop leaves the consumer group, and cancelling
    /// it mid-commit is how an offset is lost. It is already unsubscribed from
    /// the caller's point of view — the handle is consumed here — so the worst
    /// case is one lingering task that finishes its own commit and exits.
    pub async fn shutdown(self) {
        if let Err(e) = self.shutdown_tx.send(true) {
            tracing::error!(error = %e, "Failed to send Kafka consumer shutdown signal");
        }
        let timeout = self.shutdown_timeout;
        match tokio::time::timeout(timeout, self.join_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(error = %e, "Kafka consumer task panicked during shutdown");
            }
            Err(_) => {
                crate::metrics::record_error("kafka_shutdown_timeout");
                tracing::warn!(
                    timeout_ms = timeout.as_millis() as u64,
                    "Kafka consumer did not stop within its shutdown deadline; \
                     leaving it to finish in the background"
                );
            }
        }
    }

    /// Pause all assigned partitions (blocks message delivery without leaving consumer group).
    pub fn pause(&self) -> Result<(), OrionError> {
        self.with_assignment("pause", |c, a| c.pause(a))
    }

    /// Resume all assigned partitions.
    pub fn resume(&self) -> Result<(), OrionError> {
        self.with_assignment("resume", |c, a| c.resume(a))
    }

    /// Whether the consume loop has exited. After a graceful shutdown the
    /// handle itself is consumed, so a finished task behind a live handle
    /// means the consumer died — surfaced by `/health` and `/readyz` (O10).
    pub fn is_finished(&self) -> bool {
        self.join_handle.is_finished()
    }

    /// Fetch the current assignment and apply `op` to it; a consumer with no
    /// assigned partitions is a no-op. Shared plumbing for pause/resume.
    fn with_assignment(
        &self,
        op: &str,
        f: impl Fn(
            &StreamConsumer<KafkaConsumerContext>,
            &rdkafka::TopicPartitionList,
        ) -> rdkafka::error::KafkaResult<()>,
    ) -> Result<(), OrionError> {
        let assignment = self
            .consumer
            .assignment()
            .map_err(|e| OrionError::internal_from("Failed to get consumer assignment", e))?;
        if assignment.count() == 0 {
            return Ok(());
        }
        f(&self.consumer, &assignment).map_err(|e| {
            OrionError::internal_from(format!("Failed to {op} consumer partitions"), e)
        })
    }

    /// Get the set of topics this consumer is subscribed to.
    pub fn topics(&self) -> &HashSet<String> {
        &self.topics
    }

    /// Completed rebalance rounds this consumer has observed (one assignment
    /// callback each, empty assignments included).
    ///
    /// The observable that makes static membership testable (T6): when a peer
    /// restarts under the same `group.instance.id` within the session timeout
    /// it rejoins without a group rebalance, so this count on a *surviving*
    /// consumer stays put — while a dynamic member's leave + rejoin bumps it.
    pub fn rebalance_rounds(&self) -> u64 {
        self.rebalance.assign_rounds()
    }
}

/// Start the Kafka consumer in a background task.
///
/// Returns a handle for graceful shutdown. The consumer subscribes to all
/// configured topics, maps each topic to a channel, and processes messages
/// through the engine.
///
pub fn start_consumer(
    config: &KafkaIngestConfig,
    deps: ConsumerDeps,
) -> Result<ConsumerHandle, OrionError> {
    let ConsumerDeps {
        engine,
        channel_registry,
        datalogic,
        vars,
        dlq_producer,
        dlq_topic,
        instance_id,
        trace_repo,
        persistence_queue,
        max_result_size_bytes,
    } = deps;
    let mut client_config = ClientConfig::new();
    client_config
        .set("bootstrap.servers", config.brokers.join(","))
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest");
    // K9: the session timeout applies to every consumer, clustered or not —
    // it was previously set only alongside `group.instance.id`, so a
    // single-node operator configuring it got silence. Static membership is
    // the cluster-only part.
    client_config.set("session.timeout.ms", config.session_timeout_ms.to_string());
    if let Some(id) = &instance_id {
        client_config.set("group.instance.id", id);
    }
    // Applied last so kafka.extra_config can override any of the above
    super::apply_client_auth(&mut client_config, &config.auth, &config.extra_config);

    // K8: a custom context so revocations flush in-flight commits and are
    // visible to the retry loop — the default context has no rebalance hooks.
    let rebalance = Arc::new(RebalanceState::new());
    let consumer: StreamConsumer<KafkaConsumerContext> = client_config
        .create_with_context(KafkaConsumerContext::new(rebalance.clone()))
        .map_err(|e| OrionError::Internal {
            context: "Failed to create Kafka consumer".to_string(),
            source: Some(Box::new(e)),
        })?;

    // Verify broker connectivity (non-fatal — brokers may come online later)
    match consumer.fetch_metadata(None, std::time::Duration::from_secs(5)) {
        Ok(metadata) => {
            tracing::info!(
                brokers = metadata.brokers().len(),
                topics = metadata.topics().len(),
                "Kafka broker connectivity verified"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Kafka broker connectivity check failed — consumer will retry on its own"
            );
        }
    }

    // Build topic-to-channel map
    let topic_map: HashMap<String, String> = config
        .topics
        .iter()
        .map(|t| (t.topic.clone(), t.channel.clone()))
        .collect();

    let topics: Vec<&str> = config.topics.iter().map(|t| t.topic.as_str()).collect();
    consumer
        .subscribe(&topics)
        .map_err(|e| OrionError::Internal {
            context: "Failed to subscribe to Kafka topics".to_string(),
            source: Some(Box::new(e)),
        })?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let processing_timeout_ms = config.processing_timeout_ms;
    let lag_poll_interval_secs = config.lag_poll_interval_secs;

    let consumer = Arc::new(consumer);
    let topic_set: HashSet<String> = config.topics.iter().map(|t| t.topic.clone()).collect();

    let ctx = ConsumeLoopContext {
        consumer: consumer.clone(),
        topic_map,
        engine,
        channel_registry,
        datalogic,
        vars,
        dlq_producer,
        dlq_topic,
        processing_timeout_ms,
        lag_poll_interval_secs,
        rebalance: rebalance.clone(),
        retry_budget_ms: process::in_place_retry_budget_ms(&config.extra_config),
        trace_repo,
        persistence_queue,
        max_result_size_bytes,
    };
    let handle = tokio::spawn(consume_loop(ctx, shutdown_rx));

    Ok(ConsumerHandle {
        shutdown_tx,
        join_handle: handle,
        consumer,
        topics: topic_set,
        rebalance,
        shutdown_timeout: shutdown_deadline(processing_timeout_ms),
    })
}

/// Sleep for `ms`, cut short by a shutdown signal. Returns `false` only when
/// shutdown was actually signalled; a spurious `changed()` whose value is
/// still `false` (or a dropped sender) returns `true` so the caller carries
/// on. Shared by the consume loop's `recv()` backoff and the per-message
/// retry backoff in `process::process_until_committed`.
async fn sleep_or_shutdown(rx: &mut watch::Receiver<bool>, ms: u64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => true,
        _ = rx.changed() => !*rx.borrow(),
    }
}

async fn consume_loop(ctx: ConsumeLoopContext, mut shutdown_rx: watch::Receiver<bool>) {
    // Spawn consumer lag monitoring task
    let lag_handle = if ctx.lag_poll_interval_secs > 0 {
        let lag_consumer = ctx.consumer.clone();
        let lag_shutdown = shutdown_rx.clone();
        Some(tokio::spawn(poll_consumer_lag(
            lag_consumer,
            lag_shutdown,
            ctx.lag_poll_interval_secs,
        )))
    } else {
        None
    };

    tracing::info!(
        topics = ?ctx.topic_map.keys().collect::<Vec<_>>(),
        lag_poll_secs = ctx.lag_poll_interval_secs,
        "Kafka consumer started (strictly sequential processing)"
    );

    // Current backoff after consecutive `recv()` failures; cleared on success.
    let mut recv_backoff_ms: Option<u64> = None;

    // One message at a time, awaited inline: sequential processing is the
    // at-least-once contract's foundation (see the module doc).
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("Kafka consumer shutting down");
                    break;
                }
            }
            msg_result = ctx.consumer.recv() => {
                match msg_result {
                    Ok(msg) => {
                        recv_backoff_ms = None;
                        if !process_until_committed(&ctx, &msg, &mut shutdown_rx).await {
                            tracing::info!("Kafka consumer shutting down");
                            break;
                        }
                    }
                    Err(e) => {
                        // K5: this branch used to fall straight back to the top
                        // of the loop. A persistent recv() error — broker
                        // unreachable, auth failure, revoked topic
                        // authorization — then span at full CPU emitting one
                        // ERROR line per iteration, indefinitely. Back off the
                        // same way the message-level retry does, and stay
                        // interruptible by shutdown.
                        let wait = recv_backoff_ms
                            .map(process::next_backoff_ms)
                            .unwrap_or(process::INITIAL_RETRY_BACKOFF_MS);
                        recv_backoff_ms = Some(wait);
                        crate::metrics::record_error("kafka_recv");
                        tracing::error!(
                            error = %e,
                            backoff_ms = wait,
                            "Kafka consumer error; backing off before the next poll"
                        );
                        if !sleep_or_shutdown(&mut shutdown_rx, wait).await {
                            tracing::info!("Kafka consumer shutting down");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Stop the lag polling task
    if let Some(handle) = lag_handle {
        handle.abort();
    }

    tracing::info!("Kafka consumer stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_map_construction() {
        let config = crate::config::KafkaIngestConfig {
            enabled: true,
            brokers: vec!["localhost:9092".into()],
            group_id: "test".into(),
            topics: vec![
                crate::config::TopicMapping {
                    topic: "orders".into(),
                    channel: "order-channel".into(),
                },
                crate::config::TopicMapping {
                    topic: "events".into(),
                    channel: "event-channel".into(),
                },
            ],
            ..Default::default()
        };

        let topic_map: HashMap<String, String> = config
            .topics
            .iter()
            .map(|t| (t.topic.clone(), t.channel.clone()))
            .collect();

        assert_eq!(topic_map.len(), 2);
        assert_eq!(topic_map.get("orders").expect("test"), "order-channel");
        assert_eq!(topic_map.get("events").expect("test"), "event-channel");
        assert!(!topic_map.contains_key("unknown"));
    }

    /// The shutdown join is bounded, and bounded by the consumer's own
    /// per-message budget: a deadline shorter than one dispatch would report
    /// every busy consumer as wedged, and an operator's `processing_timeout_ms`
    /// must not be able to panic the addition.
    #[test]
    fn the_shutdown_deadline_outlasts_one_dispatch_and_cannot_overflow() {
        let budget = crate::config::KafkaIngestConfig::default().processing_timeout_ms;
        let deadline = shutdown_deadline(budget);
        assert!(deadline > std::time::Duration::from_millis(budget));
        assert_eq!(
            deadline,
            std::time::Duration::from_millis(budget) + SHUTDOWN_GRACE
        );

        assert!(shutdown_deadline(u64::MAX) >= std::time::Duration::from_millis(u64::MAX));
    }
}
