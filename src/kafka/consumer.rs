use std::collections::HashMap;
use std::sync::Arc;

use rdkafka::ClientConfig;
use rdkafka::Message as KafkaMessage;
use rdkafka::consumer::{Consumer, StreamConsumer};
use tokio::sync::{RwLock, watch};

use crate::config::KafkaIngestConfig;
use crate::errors::OrionError;
use crate::kafka::producer::KafkaProducer;

/// Handle for managing the Kafka consumer lifecycle.
pub struct ConsumerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl ConsumerHandle {
    /// Signal the consumer to shut down and wait for it to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join_handle.await;
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
        .map_err(|e| OrionError::Internal(format!("Failed to create Kafka consumer: {}", e)))?;

    // Build topic-to-channel map
    let topic_map: HashMap<String, String> = config
        .topics
        .iter()
        .map(|t| (t.topic.clone(), t.channel.clone()))
        .collect();

    let topics: Vec<&str> = config.topics.iter().map(|t| t.topic.as_str()).collect();
    consumer
        .subscribe(&topics)
        .map_err(|e| OrionError::Internal(format!("Failed to subscribe to Kafka topics: {}", e)))?;

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

                        // Process through engine
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

                        let engine_guard = engine.read().await;
                        match engine_guard
                            .process_message_for_channel(&channel, &mut message)
                            .await
                        {
                            Ok(()) => {
                                tracing::debug!(
                                    topic = %topic,
                                    channel = %channel,
                                    "Kafka message processed successfully"
                                );
                            }
                            Err(e) => {
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
                        drop(engine_guard);

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

        let dlq_payload = serde_json::to_string(&dlq_message).unwrap_or_default();
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
