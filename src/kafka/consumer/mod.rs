//! Kafka ingestion consumer.
//!
//! Delivery guarantee: **at-least-once**. A message's offset is committed
//! only after the message was either processed successfully or its payload
//! was confirmed written to the DLQ. On any other outcome (processing
//! failure with the DLQ disabled, or a failed DLQ write) the offset stays
//! uncommitted and the consumer retries the same message in place with
//! capped exponential backoff. Messages are handled sequentially, so no
//! later offset — which would implicitly commit earlier ones — is ever
//! committed past an unresolved failure; a restart redelivers from the
//! failed message. Enable `kafka.dlq` to avoid head-of-line blocking on
//! poison messages (e.g. payloads that will never parse).

mod dlq;
mod lag;
mod process;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::{RwLock, watch};

use crate::config::KafkaIngestConfig;
use crate::errors::OrionError;
use crate::kafka::producer::KafkaProducer;

use lag::poll_consumer_lag;
use process::process_until_committed;

/// Bundled context for the Kafka consume loop, grouping parameters that share
/// the same lifecycle and reducing positional argument count.
struct ConsumeLoopContext {
    consumer: Arc<StreamConsumer>,
    topic_map: HashMap<String, String>,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    datalogic: Arc<datalogic_rs::Engine>,
    dlq_producer: Option<Arc<KafkaProducer>>,
    dlq_topic: Option<String>,
    processing_timeout_ms: u64,
    max_inflight: usize,
    lag_poll_interval_secs: u64,
}

/// Handle for managing the Kafka consumer lifecycle.
pub struct ConsumerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<()>,
    consumer: Arc<StreamConsumer>,
    topics: HashSet<String>,
}

impl ConsumerHandle {
    /// Signal the consumer to shut down and wait for it to finish.
    pub async fn shutdown(self) {
        if let Err(e) = self.shutdown_tx.send(true) {
            tracing::error!(error = %e, "Failed to send Kafka consumer shutdown signal");
        }
        if let Err(e) = self.join_handle.await {
            tracing::error!(error = %e, "Kafka consumer task panicked during shutdown");
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

    /// Fetch the current assignment and apply `op` to it; a consumer with no
    /// assigned partitions is a no-op. Shared plumbing for pause/resume.
    fn with_assignment(
        &self,
        op: &str,
        f: impl Fn(&StreamConsumer, &rdkafka::TopicPartitionList) -> rdkafka::error::KafkaResult<()>,
    ) -> Result<(), OrionError> {
        let assignment = self
            .consumer
            .assignment()
            .map_err(|e| OrionError::Internal(format!("Failed to get consumer assignment: {e}")))?;
        if assignment.count() == 0 {
            return Ok(());
        }
        f(&self.consumer, &assignment)
            .map_err(|e| OrionError::Internal(format!("Failed to {op} consumer partitions: {e}")))
    }

    /// Get the set of topics this consumer is subscribed to.
    pub fn topics(&self) -> &HashSet<String> {
        &self.topics
    }
}

/// Start the Kafka consumer in a background task.
///
/// Returns a handle for graceful shutdown. The consumer subscribes to all
/// configured topics, maps each topic to a channel, and processes messages
/// through the engine.
///
/// `instance_id` (cluster mode) enables static group membership
/// (`group.instance.id`) plus an explicit `session.timeout.ms`, so rolling
/// restarts and reload-driven consumer restarts rejoin without a full group
/// rebalance. `None` (single node) keeps today's dynamic membership.
pub fn start_consumer(
    config: &KafkaIngestConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    datalogic: Arc<datalogic_rs::Engine>,
    dlq_producer: Option<Arc<KafkaProducer>>,
    dlq_topic: Option<String>,
    instance_id: Option<&str>,
) -> Result<ConsumerHandle, OrionError> {
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
    if let Some(id) = instance_id {
        client_config.set("group.instance.id", id);
    }
    // Applied last so kafka.extra_config can override any of the above
    super::apply_client_auth(&mut client_config, &config.auth, &config.extra_config);

    let consumer: StreamConsumer =
        client_config
            .create()
            .map_err(|e| OrionError::InternalSource {
                context: "Failed to create Kafka consumer".to_string(),
                source: Box::new(e),
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
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to subscribe to Kafka topics".to_string(),
            source: Box::new(e),
        })?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let processing_timeout_ms = config.processing_timeout_ms;
    let max_inflight = config.max_inflight;
    let lag_poll_interval_secs = config.lag_poll_interval_secs;

    let consumer = Arc::new(consumer);
    let topic_set: HashSet<String> = config.topics.iter().map(|t| t.topic.clone()).collect();

    let ctx = ConsumeLoopContext {
        consumer: consumer.clone(),
        topic_map,
        engine,
        channel_registry,
        datalogic,
        dlq_producer,
        dlq_topic,
        processing_timeout_ms,
        max_inflight,
        lag_poll_interval_secs,
    };
    let handle = tokio::spawn(consume_loop(ctx, shutdown_rx));

    Ok(ConsumerHandle {
        shutdown_tx,
        join_handle: handle,
        consumer,
        topics: topic_set,
    })
}

async fn consume_loop(ctx: ConsumeLoopContext, mut shutdown_rx: watch::Receiver<bool>) {
    let backpressure = Arc::new(tokio::sync::Semaphore::new(ctx.max_inflight));

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
        max_inflight = ctx.max_inflight,
        lag_poll_secs = ctx.lag_poll_interval_secs,
        "Kafka consumer started"
    );

    // Current backoff after consecutive `recv()` failures; cleared on success.
    let mut recv_backoff_ms: Option<u64> = None;

    loop {
        // Backpressure: wait for a permit before reading the next message.
        // This pauses the consumer when max_inflight messages are in progress.
        let _permit = match backpressure.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed
        };

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
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    tracing::info!("Kafka consumer shutting down");
                                    break;
                                }
                            }
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
}
