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
pub fn start_consumer(
    config: &KafkaIngestConfig,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    dlq_producer: Option<Arc<KafkaProducer>>,
    dlq_topic: Option<String>,
) -> Result<ConsumerHandle, OrionError> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", config.brokers.join(","))
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
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
                        let topic = msg.topic().to_string();
                        let channel = match ctx.topic_map.get(&topic) {
                            Some(ch) => ch.clone(),
                            None => {
                                tracing::warn!(topic = %topic, "No channel mapping for topic, skipping");
                                commit_offset(&ctx.consumer, &msg);
                                continue;
                            }
                        };

                        // Deserialize payload
                        let payload = match msg.payload_view::<str>() {
                            Some(Ok(text)) => text,
                            Some(Err(e)) => {
                                tracing::warn!(
                                    topic = %topic,
                                    error = %e,
                                    "Failed to decode Kafka message payload as UTF-8, skipping"
                                );
                                commit_offset(&ctx.consumer, &msg);
                                continue;
                            }
                            None => {
                                tracing::warn!(topic = %topic, "Empty Kafka message, skipping");
                                commit_offset(&ctx.consumer, &msg);
                                continue;
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
                                // Send to DLQ if enabled
                                send_to_dlq(
                                    &ctx.dlq_producer,
                                    &ctx.dlq_topic,
                                    &topic,
                                    payload.as_bytes(),
                                    &format!("JSON parse error: {}", e),
                                ).await;
                                commit_offset(&ctx.consumer, &msg);
                                continue;
                            }
                        };

                        // Extract W3C trace context from Kafka message headers
                        let _parent_cx = {
                            use opentelemetry::propagation::TextMapPropagator;
                            use opentelemetry_sdk::propagation::TraceContextPropagator;

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
                                        && let Some(value) = header.value {
                                            header_map.insert(header.key.to_string(), value.to_string());
                                        }
                                }
                            }

                            let propagator = TraceContextPropagator::new();
                            let cx = propagator.extract(&KafkaHeaderExtractor(header_map));

                            // Set extracted context as parent of the current span
                            use tracing_opentelemetry::OpenTelemetrySpanExt;
                            tracing::Span::current().set_parent(cx.clone());
                            cx
                        };

                        // Process through engine
                        let start = Instant::now();
                        let mut message = dataflow_rs::Message::from_value(&data);

                        inject_kafka_metadata(&mut message, &topic, &msg);

                        // Clone the inner Arc<Engine> and release the lock immediately
                        let engine_ref = crate::engine::acquire_engine_read(&ctx.engine).await;
                        let process_result = tokio::time::timeout(
                            std::time::Duration::from_millis(ctx.processing_timeout_ms),
                            engine_ref.process_message_for_channel(&channel, &mut message),
                        )
                        .await;

                        match process_result {
                            Err(_) => {
                                metrics::record_message(&channel, "timeout");
                                metrics::record_error("kafka_timeout");

                                tracing::error!(
                                    topic = %topic,
                                    channel = %channel,
                                    timeout_ms = ctx.processing_timeout_ms,
                                    "Kafka message processing timed out"
                                );
                                send_to_dlq(
                                    &ctx.dlq_producer,
                                    &ctx.dlq_topic,
                                    &topic,
                                    payload.as_bytes(),
                                    &format!("Processing timed out after {}ms", ctx.processing_timeout_ms),
                                ).await;
                            }
                            Ok(Err(e)) => {
                                metrics::record_message(&channel, "error");
                                metrics::record_error("kafka_processing");

                                tracing::error!(
                                    topic = %topic,
                                    channel = %channel,
                                    error = %e,
                                    "Failed to process Kafka message"
                                );
                                send_to_dlq(
                                    &ctx.dlq_producer,
                                    &ctx.dlq_topic,
                                    &topic,
                                    payload.as_bytes(),
                                    &format!("Processing error: {}", e),
                                ).await;
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

                        // Commit offset after processing
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

/// Inject Kafka-specific metadata (topic, key, partition, offset) into a dataflow message.
fn inject_kafka_metadata(
    message: &mut dataflow_rs::Message,
    topic: &str,
    msg: &rdkafka::message::BorrowedMessage<'_>,
) {
    use rdkafka::Message as KafkaMsg;
    message.metadata_mut()["kafka_topic"] = serde_json::Value::String(topic.to_string());
    if let Some(key) = msg.key().and_then(|k| std::str::from_utf8(k).ok()) {
        message.metadata_mut()["kafka_key"] = serde_json::Value::String(key.to_string());
    }
    message.metadata_mut()["kafka_partition"] = serde_json::json!(msg.partition());
    message.metadata_mut()["kafka_offset"] = serde_json::json!(msg.offset());
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

        let dlq_payload = serde_json::to_string(&dlq_message).unwrap_or_else(|e| {
            tracing::error!(error = %e, "Failed to serialize DLQ message");
            format!(
                r#"{{"source_topic":"{}","error":"serialization failed","timestamp":"{}"}}"#,
                source_topic,
                chrono::Utc::now().to_rfc3339()
            )
        });
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
        };

        let topic_map: HashMap<String, String> = config
            .topics
            .iter()
            .map(|t| (t.topic.clone(), t.channel.clone()))
            .collect();

        assert_eq!(topic_map.len(), 2);
        assert_eq!(topic_map.get("orders").unwrap(), "order-channel");
        assert_eq!(topic_map.get("events").unwrap(), "event-channel");
        assert!(topic_map.get("unknown").is_none());
    }

    #[test]
    fn test_dlq_message_format() {
        let payload = br#"{"data": {"broken": true}}"#;
        let msg = build_dlq_message("test-topic", payload, "JSON parse error");

        assert_eq!(msg["source_topic"], "test-topic");
        assert_eq!(msg["error"], "JSON parse error");
        assert_eq!(msg["original_payload"], r#"{"data": {"broken": true}}"#);
        // Timestamp should be a valid RFC3339 string
        let ts = msg["timestamp"].as_str().unwrap();
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
        let original = msg["original_payload"].as_str().unwrap();
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
