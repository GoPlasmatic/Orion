use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use rdkafka::ClientConfig;
use rdkafka::Message as _;
use rdkafka::TopicPartitionList;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::{RwLock, watch};

use crate::config::KafkaIngestConfig;
use crate::errors::OrionError;
use crate::kafka::producer::KafkaProducer;
use crate::metrics;

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

use rdkafka::message::Headers;

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
        let assignment = self
            .consumer
            .assignment()
            .map_err(|e| OrionError::Internal(format!("Failed to get consumer assignment: {e}")))?;
        if assignment.count() == 0 {
            return Ok(());
        }
        self.consumer.pause(&assignment).map_err(|e| {
            OrionError::Internal(format!("Failed to pause consumer partitions: {e}"))
        })?;
        Ok(())
    }

    /// Resume all assigned partitions.
    pub fn resume(&self) -> Result<(), OrionError> {
        let assignment = self
            .consumer
            .assignment()
            .map_err(|e| OrionError::Internal(format!("Failed to get consumer assignment: {e}")))?;
        if assignment.count() == 0 {
            return Ok(());
        }
        self.consumer.resume(&assignment).map_err(|e| {
            OrionError::Internal(format!("Failed to resume consumer partitions: {e}"))
        })?;
        Ok(())
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
    if let Some(id) = instance_id {
        client_config
            .set("group.instance.id", id)
            .set("session.timeout.ms", config.session_timeout_ms.to_string());
    }
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

/// Decode + parse + dispatch a single Kafka message. Wraps the entire
/// per-message lifecycle: payload UTF-8 decode (skips on failure), topic →
/// channel lookup (skips unmapped topics), JSON parse (DLQ on failure),
/// W3C trace context extraction, engine dispatch with timeout, and the
/// 4-way match on the processing outcome (timeout / engine error /
/// workflow errors / success). The outer `consume_loop` is responsible
/// for backpressure, shutdown, and offset commit after this returns.
async fn process_one_kafka_message(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
) {
    let topic = msg.topic().to_string();
    let channel = match ctx.topic_map.get(&topic) {
        Some(ch) => ch.clone(),
        None => {
            tracing::warn!(topic = %topic, "No channel mapping for topic, skipping");
            return;
        }
    };

    let payload = match msg.payload_view::<str>() {
        Some(Ok(text)) => text,
        Some(Err(e)) => {
            tracing::warn!(
                topic = %topic,
                error = %e,
                "Failed to decode Kafka message payload as UTF-8, skipping"
            );
            return;
        }
        None => {
            tracing::warn!(topic = %topic, "Empty Kafka message, skipping");
            return;
        }
    };

    let data: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                topic = %topic,
                error = %e,
                "Failed to parse Kafka message as JSON, skipping"
            );
            send_to_dlq(
                &ctx.dlq_producer,
                &ctx.dlq_topic,
                &topic,
                payload.as_bytes(),
                &format!("JSON parse error: {e}"),
            )
            .await;
            return;
        }
    };

    // Extract W3C trace context from Kafka message headers and attach it as
    // parent of the current tracing span (held for the rest of this scope).
    let _parent_cx = extract_kafka_trace_context(msg);

    // S1: apply the target channel's validation_logic before dispatch,
    // mirroring the HTTP ingress path. CORS / dedup / response cache are
    // HTTP-transport concerns and don't apply here. Failures are not
    // silently dropped — they record metrics, log, and route to the DLQ
    // when one is configured.
    let metadata = kafka_metadata_value(&topic, msg);
    let channel_runtime = ctx.channel_registry.get_by_name(&channel).await;
    if let Err(e) = crate::channel::guards::validate_input(
        &channel,
        &channel_runtime,
        &data,
        &metadata,
        &ctx.datalogic,
    ) {
        report_failure_and_dlq(
            ctx,
            FailureReport {
                channel: &channel,
                topic: &topic,
                payload: payload.as_bytes(),
                message_status: "error",
                error_kind: "kafka_validation",
                log_msg: "Kafka message rejected by channel validation_logic",
                dlq_reason: &format!("Validation failed: {e}"),
            },
        )
        .await;
        return;
    }

    let start = Instant::now();
    let mut message = dataflow_rs::Message::from_value(&data);
    crate::engine::utils::merge_metadata(&mut message, &metadata);

    // Clone the inner Arc<Engine> and release the lock immediately.
    let engine_ref = crate::engine::acquire_engine_read(&ctx.engine).await;
    let process_result = tokio::time::timeout(
        std::time::Duration::from_millis(ctx.processing_timeout_ms),
        engine_ref.process_message_for_channel(&channel, &mut message),
    )
    .await;

    match process_result {
        Err(_) => {
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "timeout",
                    error_kind: "kafka_timeout",
                    log_msg: "Kafka message processing timed out",
                    dlq_reason: &format!(
                        "Processing timed out after {}ms",
                        ctx.processing_timeout_ms
                    ),
                },
            )
            .await;
        }
        Ok(Err(e)) => {
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "kafka_processing",
                    log_msg: "Failed to process Kafka message",
                    dlq_reason: &format!("Processing error: {e}"),
                },
            )
            .await;
        }
        Ok(Ok(())) if message.has_errors() => {
            // v3 contract: workflow failures are pushed to
            // message.errors() while the outer Result stays Ok.
            let summary = message
                .errors()
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "kafka_processing",
                    log_msg: "Kafka message processed with workflow errors",
                    dlq_reason: &format!("Workflow errors: {summary}"),
                },
            )
            .await;
        }
        Ok(Ok(())) => {
            let duration = start.elapsed().as_secs_f64();
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration);
            tracing::debug!(
                topic = %topic,
                channel = %channel,
                "Kafka message processed successfully"
            );
        }
    }
}

/// Extract a W3C trace context from a Kafka message's headers and attach
/// it as the parent of the current tracing span. Returns the propagated
/// `opentelemetry::Context` so the caller can keep it in scope.
fn extract_kafka_trace_context(
    msg: &rdkafka::message::BorrowedMessage<'_>,
) -> opentelemetry::Context {
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    struct KafkaHeaderExtractor(HashMap<String, String>);
    impl opentelemetry::propagation::Extractor for KafkaHeaderExtractor {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).map(|v| v.as_str())
        }
        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(|k| k.as_str()).collect()
        }
    }

    let mut header_map = HashMap::new();
    if let Some(headers) = msg.headers() {
        for idx in 0..headers.count() {
            if let Ok(header) = headers.get_as::<str>(idx)
                && let Some(value) = header.value
            {
                header_map.insert(header.key.to_string(), value.to_string());
            }
        }
    }

    let propagator = TraceContextPropagator::new();
    let cx = propagator.extract(&KafkaHeaderExtractor(header_map));
    let _ = tracing::Span::current().set_parent(cx.clone());
    cx
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
                        process_one_kafka_message(&ctx, &msg).await;
                        commit_offset(&ctx.consumer, &msg);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Kafka consumer error");
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

/// Commit the offset for a consumed message, logging any errors.
fn commit_offset(consumer: &StreamConsumer, msg: &rdkafka::message::BorrowedMessage<'_>) {
    use rdkafka::consumer::CommitMode;
    if let Err(e) = consumer.commit_message(msg, CommitMode::Async) {
        tracing::error!(error = %e, "Failed to commit Kafka offset");
    }
}

/// Build the Kafka-specific metadata object (topic, key, partition, offset)
/// for an ingested message. Used both as validation_logic context and as the
/// metadata merged into the dispatched dataflow message, so validation sees
/// exactly what the workflow will see.
fn kafka_metadata_value(
    topic: &str,
    msg: &rdkafka::message::BorrowedMessage<'_>,
) -> serde_json::Value {
    use rdkafka::Message as KafkaMsg;

    let mut meta = serde_json::json!({
        "kafka_topic": topic,
        "kafka_partition": msg.partition(),
        "kafka_offset": msg.offset(),
    });
    if let Some(key) = msg.key().and_then(|k| std::str::from_utf8(k).ok()) {
        meta["kafka_key"] = serde_json::json!(key);
    }
    meta
}

/// Periodically poll committed offsets and high watermarks to compute consumer lag.
async fn poll_consumer_lag(
    consumer: Arc<StreamConsumer>,
    mut shutdown_rx: watch::Receiver<bool>,
    interval_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    // Skip the first immediate tick — let the consumer establish itself first
    interval.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            _ = interval.tick() => {
                let consumer = consumer.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let committed = match consumer.committed(std::time::Duration::from_secs(5)) {
                        Ok(tpl) => tpl,
                        Err(e) => {
                            tracing::debug!(error = %e, "Failed to fetch committed offsets for lag metric");
                            return;
                        }
                    };
                    report_lag_for_partitions(&consumer, &committed);
                }).await;
            }
        }
    }
}

/// Compute and report lag for each topic-partition in the committed offsets list.
fn report_lag_for_partitions(consumer: &StreamConsumer, committed: &TopicPartitionList) {
    for elem in committed.elements() {
        let topic = elem.topic();
        let partition = elem.partition();

        let committed_offset = match elem.offset() {
            rdkafka::Offset::Offset(n) => n,
            rdkafka::Offset::Invalid | rdkafka::Offset::Beginning => 0,
            _ => continue, // Stored, End, etc. — skip
        };

        match consumer.fetch_watermarks(topic, partition, std::time::Duration::from_secs(5)) {
            Ok((_low, high)) => {
                let lag = (high - committed_offset).max(0);
                metrics::set_kafka_consumer_lag(topic, partition, lag as f64);
            }
            Err(e) => {
                tracing::debug!(
                    topic = %topic,
                    partition = partition,
                    error = %e,
                    "Failed to fetch watermarks for lag metric"
                );
            }
        }
    }
}

/// Per-failure descriptor for [`report_failure_and_dlq`]. Bundles the
/// message identity (channel/topic/payload) with the metric and log
/// labels for one of the consume loop's failure branches.
struct FailureReport<'a> {
    channel: &'a str,
    topic: &'a str,
    payload: &'a [u8],
    message_status: &'static str,
    error_kind: &'static str,
    log_msg: &'a str,
    dlq_reason: &'a str,
}

/// Record failure metrics and ship the original payload to the DLQ.
/// Used by the 4 failure branches in the consume loop (UTF-8 decode,
/// JSON parse, processing timeout, workflow error) to keep metric / log /
/// DLQ behaviour consistent.
async fn report_failure_and_dlq(ctx: &ConsumeLoopContext, failure: FailureReport<'_>) {
    metrics::record_message(failure.channel, failure.message_status);
    metrics::record_error(failure.error_kind);
    tracing::error!(
        topic = %failure.topic,
        channel = %failure.channel,
        error = %failure.dlq_reason,
        "{}",
        failure.log_msg
    );
    send_to_dlq(
        &ctx.dlq_producer,
        &ctx.dlq_topic,
        failure.topic,
        failure.payload,
        failure.dlq_reason,
    )
    .await;
}

/// Build a DLQ envelope message from error context.
fn build_dlq_message(source_topic: &str, payload: &[u8], error: &str) -> serde_json::Value {
    serde_json::json!({
        "source_topic": source_topic,
        "error": error,
        "original_payload": String::from_utf8_lossy(payload),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// Send a failed message to the dead-letter queue if configured.
async fn send_to_dlq(
    producer: &Option<Arc<KafkaProducer>>,
    dlq_topic: &Option<String>,
    source_topic: &str,
    payload: &[u8],
    error: &str,
) {
    if let (Some(producer), Some(topic)) = (producer, dlq_topic) {
        let dlq_message = build_dlq_message(source_topic, payload, error);

        // Infallible: the envelope is a `serde_json::Value` built from
        // string-keyed literals via `json!`, which cannot fail to serialise.
        let dlq_payload =
            serde_json::to_string(&dlq_message).expect("DLQ envelope is always serialisable");
        if let Err(e) = producer
            .send(topic, Some(source_topic), dlq_payload.as_bytes())
            .await
        {
            tracing::error!(
                dlq_topic = %topic,
                error = %e,
                "Failed to send message to DLQ"
            );
        } else {
            tracing::debug!(
                dlq_topic = %topic,
                source_topic = %source_topic,
                "Message sent to DLQ"
            );
        }
    }
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
            dlq: crate::config::DlqConfig::default(),
            processing_timeout_ms: 60_000,
            max_inflight: 10,
            lag_poll_interval_secs: 30,
            session_timeout_ms: 45_000,
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

    #[test]
    fn test_dlq_message_format() {
        let payload = br#"{"data": {"broken": true}}"#;
        let msg = build_dlq_message("test-topic", payload, "JSON parse error");

        assert_eq!(msg["source_topic"], "test-topic");
        assert_eq!(msg["error"], "JSON parse error");
        assert_eq!(msg["original_payload"], r#"{"data": {"broken": true}}"#);
        // Timestamp should be a valid RFC3339 string
        let ts = msg["timestamp"].as_str().expect("test");
        assert!(ts.contains("T"));
        assert!(ts.ends_with('Z') || ts.contains('+'));
    }

    #[test]
    fn test_dlq_message_invalid_utf8_payload() {
        let payload: &[u8] = &[0xFF, 0xFE, 0xFD];
        let msg = build_dlq_message("bad-topic", payload, "UTF-8 decode error");

        assert_eq!(msg["source_topic"], "bad-topic");
        assert_eq!(msg["error"], "UTF-8 decode error");
        // Lossy conversion should produce replacement characters
        let original = msg["original_payload"].as_str().expect("test");
        assert!(original.contains('\u{FFFD}'));
    }

    #[test]
    fn test_dlq_message_empty_payload() {
        let msg = build_dlq_message("topic", b"", "empty message");

        assert_eq!(msg["source_topic"], "topic");
        assert_eq!(msg["error"], "empty message");
        assert_eq!(msg["original_payload"], "");
    }
}
