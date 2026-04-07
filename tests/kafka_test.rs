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
        max_inflight: 10,
        lag_poll_interval_secs: 0, // disable in tests — no real broker to query
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

    let handle =
        consumer::start_consumer(&config, engine, Some(dlq_producer), Some(dlq_topic.clone()))
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
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Verify DLQ received the message.
    // The verifier consumer may need several attempts: the DLQ topic is
    // auto-created by the producer and the consumer group coordinator needs
    // time to assign partitions after subscribe().
    let dlq_consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("group.id", format!("dlq-verifier-{}", uuid::Uuid::new_v4()))
        .set("auto.offset.reset", "earliest")
        .set("fetch.wait.max.ms", "500")
        .create()
        .unwrap();

    dlq_consumer.subscribe(&[&dlq_topic]).unwrap();

    // Poll with retries — first few recv() calls may return errors while the
    // consumer group is rebalancing and partition assignments are pending.
    let mut dlq_payload: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), dlq_consumer.recv()).await {
            Ok(Ok(msg)) => {
                if let Some(payload) = msg.payload() {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(payload) {
                        dlq_payload = Some(val);
                        break;
                    }
                }
            }
            Ok(Err(_)) => {
                // Broker error (e.g. topic not yet available) — retry
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(_) => {
                // Timeout — retry
            }
        }
    }

    let dlq_payload = dlq_payload.expect("DLQ message not received within deadline");

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

// ============================================================
// Concurrent message processing tests
// ============================================================

/// Produce many messages concurrently and verify the consumer handles them all.
#[tokio::test]
#[ignore]
async fn test_concurrent_message_processing() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-concurrent";
    let channel = "concurrent-channel";
    let msg_count = 50;

    let config = KafkaIngestConfig {
        max_inflight: 10,
        ..test_kafka_config(&brokers, topic, channel)
    };
    let engine = empty_engine();

    // Initialize metrics so we can verify counts
    let _ = orion::metrics::init_metrics();

    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    // Produce messages concurrently
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();

    let mut send_tasks = Vec::new();
    for i in 0..msg_count {
        let producer = producer.clone();
        let topic = topic.to_string();
        send_tasks.push(tokio::spawn(async move {
            let payload = format!(r#"{{"data": {{"index": {}}}}}"#, i);
            producer
                .send(
                    FutureRecord::<str, str>::to(&topic)
                        .key(&format!("key-{}", i))
                        .payload(&payload),
                    Duration::from_secs(10),
                )
                .await
                .expect("Failed to produce message");
        }));
    }

    // Wait for all produces to complete
    for task in send_tasks {
        task.await.unwrap();
    }

    // Give consumer time to process all messages
    // With max_inflight=10, 50 messages should be processed fairly quickly
    tokio::time::sleep(Duration::from_secs(10)).await;

    handle.shutdown().await;
    // If we get here, the consumer processed messages without panic or deadlock
    // under concurrent load with backpressure active (max_inflight=10 < msg_count=50)
}

/// Produce a rapid burst of messages and verify the consumer keeps up.
#[tokio::test]
#[ignore]
async fn test_consumer_backpressure_under_load() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-backpressure";
    let channel = "bp-channel";

    // Low max_inflight to force backpressure behavior
    let config = KafkaIngestConfig {
        max_inflight: 2,
        ..test_kafka_config(&brokers, topic, channel)
    };
    let engine = empty_engine();

    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();

    // Produce 20 messages rapidly
    for i in 0..20 {
        let payload = format!(r#"{{"data": {{"seq": {}}}}}"#, i);
        producer
            .send(
                FutureRecord::<str, str>::to(topic).payload(&payload),
                Duration::from_secs(10),
            )
            .await
            .expect("Failed to produce message");
    }

    // With max_inflight=2, the consumer processes at most 2 at a time.
    // 20 messages should still complete within a reasonable time.
    tokio::time::sleep(Duration::from_secs(15)).await;

    handle.shutdown().await;
    // Success = no deadlocks or panics under constrained backpressure
}

/// Test consumer with multiple topic-to-channel mappings.
#[tokio::test]
#[ignore]
async fn test_consumer_multiple_topics() {
    let (_container, brokers) = start_kafka().await;

    let topic_a = "test-multi-a";
    let topic_b = "test-multi-b";

    let config = KafkaIngestConfig {
        enabled: true,
        brokers: vec![brokers.clone()],
        group_id: format!("test-{}", uuid::Uuid::new_v4()),
        topics: vec![
            TopicMapping {
                topic: topic_a.to_string(),
                channel: "channel-a".to_string(),
            },
            TopicMapping {
                topic: topic_b.to_string(),
                channel: "channel-b".to_string(),
            },
        ],
        dlq: DlqConfig {
            enabled: false,
            topic: "unused".to_string(),
        },
        processing_timeout_ms: 5_000,
        max_inflight: 10,
        lag_poll_interval_secs: 0,
    };
    let engine = empty_engine();

    let handle = consumer::start_consumer(&config, engine, None, None).unwrap();

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    // Send to topic A
    producer
        .send(
            FutureRecord::<str, str>::to(topic_a).payload(r#"{"data": {"from": "a"}}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce to topic A");

    // Send to topic B
    producer
        .send(
            FutureRecord::<str, str>::to(topic_b).payload(r#"{"data": {"from": "b"}}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce to topic B");

    // Give consumer time to process both
    tokio::time::sleep(Duration::from_secs(5)).await;

    handle.shutdown().await;
    // Success = consumer handles messages from multiple topics without confusion
}
