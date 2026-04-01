use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rdkafka::ClientConfig;
use rdkafka::Message as KafkaMessage;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::{RwLock, watch};

use crate::config::KafkaIngestConfig;
use crate::errors::OrionError;
use crate::kafka::producer::KafkaProducer;
use crate::metrics;

#[cfg(feature = "otel")]
use rdkafka::message::Headers;

/// Handle for managing the Kafka consumer lifecycle.
pub struct ConsumerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<()>,
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

    let handle = tokio::spawn(consume_loop(
        consumer,
        topic_map,
        engine,
        dlq_producer,
        dlq_topic,
        shutdown_rx,
    ));

    Ok(ConsumerHandle {
        shutdown_tx,
        join_handle: handle,
    })
}

async fn consume_loop(
    consumer: StreamConsumer,
    topic_map: HashMap<String, String>,
    engine: Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    dlq_producer: Option<Arc<KafkaProducer>>,
    dlq_topic: Option<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    use rdkafka::consumer::CommitMode;

    tracing::info!(
        topics = ?topic_map.keys().collect::<Vec<_>>(),
        "Kafka consumer started"
    );

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("Kafka consumer shutting down");
                    break;
                }
            }
            msg_result = consumer.recv() => {
                match msg_result {
                    Ok(msg) => {
                        let topic = msg.topic().to_string();
                        let channel = match topic_map.get(&topic) {
                            Some(ch) => ch.clone(),
                            None => {
                                tracing::warn!(topic = %topic, "No channel mapping for topic, skipping");
                                if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
                                    tracing::error!(error = %e, "Failed to commit offset");
                                }
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
                                if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
                                    tracing::error!(error = %e, "Failed to commit offset");
                                }
                                continue;
                            }
                            None => {
                                tracing::warn!(topic = %topic, "Empty Kafka message, skipping");
                                if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
                                    tracing::error!(error = %e, "Failed to commit offset");
                                }
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
                                    &dlq_producer,
                                    &dlq_topic,
                                    &topic,
                                    payload.as_bytes(),
                                    &format!("JSON parse error: {}", e),
                                ).await;
                                if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
                                    tracing::error!(error = %e, "Failed to commit offset");
                                }
                                continue;
                            }
                        };

                        // Extract W3C trace context from Kafka message headers
                        #[cfg(feature = "otel")]
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

                        // Add Kafka metadata
                        message.metadata_mut()["kafka_topic"] =
                            serde_json::Value::String(topic.clone());
                        if let Some(key) = msg.key().and_then(|k| std::str::from_utf8(k).ok()) {
                            message.metadata_mut()["kafka_key"] =
                                serde_json::Value::String(key.to_string());
                        }
                        message.metadata_mut()["kafka_partition"] =
                            serde_json::json!(msg.partition());
                        message.metadata_mut()["kafka_offset"] =
                            serde_json::json!(msg.offset());

                        // Clone the inner Arc<Engine> and release the lock immediately
                        let engine_ref = engine.read().await.clone();
                        match engine_ref
                            .process_message_for_channel(&channel, &mut message)
                            .await
                        {
                            Ok(()) => {
                                let duration = start.elapsed().as_secs_f64();
                                metrics::record_message(&channel, "ok");
                                metrics::record_message_duration(&channel, duration);

                                tracing::debug!(
                                    topic = %topic,
                                    channel = %channel,
                                    "Kafka message processed successfully"
                                );
                            }
                            Err(e) => {
                                metrics::record_message(&channel, "error");
                                metrics::record_error("kafka_processing");

                                tracing::error!(
                                    topic = %topic,
                                    channel = %channel,
                                    error = %e,
                                    "Failed to process Kafka message"
                                );
                                send_to_dlq(
                                    &dlq_producer,
                                    &dlq_topic,
                                    &topic,
                                    payload.as_bytes(),
                                    &format!("Processing error: {}", e),
                                ).await;
                            }
                        }

                        // Commit offset after processing
                        if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
                            tracing::error!(error = %e, "Failed to commit Kafka offset");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Kafka consumer error");
                    }
                }
            }
        }
    }

    tracing::info!("Kafka consumer stopped");
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
        // Wrap original message with error metadata
        let dlq_message = serde_json::json!({
            "source_topic": source_topic,
            "error": error,
            "original_payload": String::from_utf8_lossy(payload),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

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
