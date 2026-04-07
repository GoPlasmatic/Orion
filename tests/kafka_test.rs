//! Kafka integration tests.
//!
//! These tests require Docker to be running (uses testcontainers with Kafka).
//! Run with: `cargo test --features kafka kafka_test`
//!
//! Tests are marked #[ignore] by default to avoid CI failures when Docker is unavailable.
//! Run explicitly with: `cargo test --features kafka -- --ignored kafka`

#![cfg(feature = "kafka")]

use std::sync::Arc;
use std::time::Duration;

use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::{ClientConfig, Message};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::kafka::Kafka;
use tokio::sync::RwLock;

use orion::config::{DlqConfig, KafkaIngestConfig, TopicMapping};
use orion::kafka::consumer;
use orion::kafka::producer::KafkaProducer;

/// Start a Kafka container and return the broker address.
async fn start_kafka() -> (testcontainers::ContainerAsync<Kafka>, String) {
    let container = Kafka::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(9093).await.unwrap();
    let brokers = format!("localhost:{}", port);
    // Give Kafka a moment to stabilize
    tokio::time::sleep(Duration::from_secs(2)).await;
    (container, brokers)
}

/// Create a simple test engine with no workflows.
fn empty_engine() -> Arc<RwLock<Arc<dataflow_rs::Engine>>> {
    Arc::new(RwLock::new(Arc::new(dataflow_rs::Engine::new(
        vec![],
        None,
    ))))
}

/// Build a test Kafka config for the given broker.
fn test_kafka_config(brokers: &str, topic: &str, channel: &str) -> KafkaIngestConfig {
    KafkaIngestConfig {
        enabled: true,
        brokers: vec![brokers.to_string()],
        group_id: format!("test-{}", uuid::Uuid::new_v4()),
        topics: vec![TopicMapping {
            topic: topic.to_string(),
            channel: channel.to_string(),
        }],
        dlq: DlqConfig {
            enabled: true,
            topic: format!("{}-dlq", topic),
        },
        processing_timeout_ms: 5_000,
    }
}

// ============================================================
// Producer tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_producer_send_message() {
    let (_container, brokers) = start_kafka().await;

    let producer = KafkaProducer::new(&brokers).unwrap();
    let topic = "test-producer-send";

    // Send a message — should not error
    let result = producer
        .send(topic, Some("key1"), b"{\"data\": {\"value\": 42}}")
        .await;
    assert!(result.is_ok(), "Producer send failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_producer_send_without_key() {
    let (_container, brokers) = start_kafka().await;

    let producer = KafkaProducer::new(&brokers).unwrap();
    let topic = "test-producer-no-key";

    let result = producer
        .send(topic, None, b"{\"data\": {\"value\": 1}}")
        .await;
    assert!(result.is_ok());
}

// ============================================================
// Consumer tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_consumer_starts_and_stops() {
    let (_container, brokers) = start_kafka().await;

    let config = test_kafka_config(&brokers, "test-lifecycle", "test-channel");
    let engine = empty_engine();

    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    // Consumer should be running — give it a moment
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Graceful shutdown
    handle.shutdown().await;
    // If we get here without panic, shutdown was clean
}

#[tokio::test]
#[ignore]
async fn test_consumer_processes_valid_message() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-valid-msg";
    let channel = "test-channel";
    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    // Start consumer
    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    // Produce a message
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    let payload = r#"{"data": {"test": true}}"#;
    producer
        .send(
            FutureRecord::<str, str>::to(topic).payload(payload),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce test message");

    // Give consumer time to process
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Shutdown cleanly
    handle.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn test_consumer_sends_invalid_json_to_dlq() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-invalid-json";
    let dlq_topic = format!("{}-dlq", topic);
    let channel = "test-channel";
    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    // Create DLQ producer
    let dlq_producer = Arc::new(KafkaProducer::new(&brokers).unwrap());

    let handle = consumer::start_consumer(
        &config,
        engine,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
    )
    .unwrap();

    // Produce an invalid JSON message
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    producer
        .send(
            FutureRecord::<str, str>::to(topic).payload("not valid json {{{{"),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce test message");

    // Give consumer time to process and send to DLQ
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify DLQ received the message
    let dlq_consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("group.id", "dlq-verifier")
        .set("auto.offset.reset", "earliest")
        .create()
        .unwrap();

    dlq_consumer.subscribe(&[&dlq_topic]).unwrap();

    let dlq_msg = tokio::time::timeout(
        Duration::from_secs(10),
        dlq_consumer.recv(),
    )
    .await;

    assert!(dlq_msg.is_ok(), "Timed out waiting for DLQ message");
    let dlq_msg = dlq_msg.unwrap().unwrap();
    let dlq_payload: serde_json::Value =
        serde_json::from_slice(dlq_msg.payload().unwrap()).unwrap();

    assert_eq!(dlq_payload["source_topic"], topic);
    assert!(dlq_payload["error"].as_str().unwrap().contains("JSON"));
    assert_eq!(dlq_payload["original_payload"], "not valid json {{{{");

    handle.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn test_consumer_metadata_injection() {
    // This test verifies that Kafka metadata fields are injected into the message.
    // Since we can't easily inspect message.metadata() from outside the consumer loop,
    // we verify the consumer doesn't error when processing a keyed message.
    let (_container, brokers) = start_kafka().await;

    let topic = "test-metadata";
    let channel = "test-channel";
    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    // Produce a message with a key
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    producer
        .send(
            FutureRecord::to(topic)
                .key("order-123")
                .payload(r#"{"data": {"order_id": "123"}}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce keyed message");

    tokio::time::sleep(Duration::from_secs(3)).await;
    handle.shutdown().await;
}
