//! Kafka integration tests.
//!
//! These tests require Docker to be running (uses testcontainers with Kafka).
//! Run with: `cargo test kafka_test`
//!
//! Tests are marked #[ignore] by default to avoid CI failures when Docker is unavailable.
//! Run explicitly with: `cargo test -- --ignored kafka`

use std::sync::Arc;
use std::time::Duration;

use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::{ClientConfig, Message};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::kafka::Kafka;
use tokio::sync::RwLock;

use orion::config::{DlqConfig, KafkaAuthConfig, KafkaIngestConfig, TopicMapping};
use orion::kafka::consumer;
use orion::kafka::producer::KafkaProducer;

/// Build an Orion producer with default (plaintext) auth for tests.
fn plain_producer(brokers: &str) -> KafkaProducer {
    KafkaProducer::new(
        brokers,
        &KafkaAuthConfig::default(),
        &std::collections::HashMap::new(),
    )
    .unwrap()
}

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
    Arc::new(RwLock::new(Arc::new(
        dataflow_rs::Engine::builder().build().unwrap(),
    )))
}

/// Empty channel registry for consumer tests (no per-channel guards configured).
fn test_registry() -> Arc<orion::channel::ChannelRegistry> {
    Arc::new(orion::channel::ChannelRegistry::new())
}

/// Shared datalogic engine for consumer tests.
fn test_datalogic() -> Arc<datalogic_rs::Engine> {
    Arc::new(datalogic_rs::Engine::new())
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
        lag_poll_interval_secs: 0, // disable in tests — no real broker to query
        ..Default::default()
    }
}

// ============================================================
// Producer tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_producer_send_message() {
    let (_container, brokers) = start_kafka().await;

    let producer = plain_producer(&brokers);
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

    let producer = plain_producer(&brokers);
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

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    // Let the consumer reach its subscribe/poll loop before stopping it, so
    // shutdown is exercised against a running consumer rather than a task
    // that never started.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The lifecycle contract: shutdown of a running consumer completes
    // promptly instead of wedging on the poll loop.
    shutdown_within(handle, 10).await;
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
    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

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

    // The offset is committed only after successful processing, so this
    // proves the message was consumed and processed — not just produced.
    wait_for_committed(&brokers, &config.group_id, topic, 1, 1, 30).await;

    shutdown_within(handle, 10).await;
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
    let dlq_producer = Arc::new(plain_producer(&brokers));

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
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
    tokio::time::sleep(Duration::from_secs(5)).await;

    let dlq_payload = wait_for_dlq_message(&brokers, &dlq_topic, 20)
        .await
        .expect("DLQ message not received within deadline");

    assert_eq!(dlq_payload["source_topic"], topic);
    assert!(dlq_payload["error"].as_str().unwrap().contains("JSON"));
    assert_eq!(dlq_payload["original_payload"], "not valid json {{{{");

    handle.shutdown().await;
}

/// Poll a fresh consumer group on `dlq_topic` until a JSON DLQ envelope
/// arrives or the deadline passes.
///
/// The verifier consumer may need several attempts: the DLQ topic is
/// auto-created by the producer and the consumer group coordinator needs
/// time to assign partitions after subscribe().
async fn wait_for_dlq_message(
    brokers: &str,
    dlq_topic: &str,
    deadline_secs: u64,
) -> Option<serde_json::Value> {
    let dlq_consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", format!("dlq-verifier-{}", uuid::Uuid::new_v4()))
        .set("auto.offset.reset", "earliest")
        .set("fetch.wait.max.ms", "500")
        .create()
        .unwrap();

    dlq_consumer.subscribe(&[dlq_topic]).unwrap();

    // Poll with retries — first few recv() calls may return errors while the
    // consumer group is rebalancing and partition assignments are pending.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), dlq_consumer.recv()).await {
            Ok(Ok(msg)) => {
                if let Some(payload) = msg.payload()
                    && let Ok(val) = serde_json::from_slice::<serde_json::Value>(payload)
                {
                    return Some(val);
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
    None
}

/// Sum of the committed offsets for `group_id` across the first `partitions`
/// partitions of `topic`; a partition with no commit counts as 0, and -1 is
/// returned while the broker is unreachable so callers can retry.
///
/// This is the consumer's public side effect: `enable.auto.commit` is off and
/// an offset is committed only after `process_until_committed` succeeds, so
/// "committed total == messages produced" proves consume + process + commit —
/// the thing a fixed sleep never established.
async fn committed_total(brokers: &str, group_id: &str, topic: &str, partitions: i32) -> i64 {
    let brokers = brokers.to_string();
    let group_id = group_id.to_string();
    let topic = topic.to_string();
    // committed_offsets() blocks; keep it off the runtime that is driving
    // the consumer under test.
    tokio::task::spawn_blocking(move || {
        use rdkafka::TopicPartitionList;
        use rdkafka::consumer::BaseConsumer;
        let probe: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("group.id", &group_id)
            .create()
            .unwrap();
        let mut tpl = TopicPartitionList::new();
        for p in 0..partitions {
            tpl.add_partition(&topic, p);
        }
        match probe.committed_offsets(tpl, Duration::from_secs(5)) {
            Ok(committed) => committed
                .elements()
                .iter()
                .map(|e| match e.offset() {
                    rdkafka::Offset::Offset(n) => n,
                    _ => 0,
                })
                .sum(),
            Err(_) => -1,
        }
    })
    .await
    .unwrap()
}

/// Poll until `group_id` has committed `expected` offsets in total across
/// `topic`, or panic at the deadline with the last observed count.
async fn wait_for_committed(
    brokers: &str,
    group_id: &str,
    topic: &str,
    partitions: i32,
    expected: i64,
    deadline_secs: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    let mut last = -1;
    while tokio::time::Instant::now() < deadline {
        last = committed_total(brokers, group_id, topic, partitions).await;
        if last == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!(
        "group '{group_id}' committed {last}/{expected} offsets on '{topic}' within \
         {deadline_secs}s — messages were produced but not processed to commit"
    );
}

/// Shutdown must complete promptly: a wedged worker would otherwise burn the
/// whole test timeout and read as a slow test instead of a bug.
async fn shutdown_within(handle: consumer::ConsumerHandle, secs: u64) {
    tokio::time::timeout(Duration::from_secs(secs), handle.shutdown())
        .await
        .expect("consumer shutdown did not complete in time");
}

#[tokio::test]
#[ignore]
async fn test_consumer_sends_invalid_utf8_to_dlq() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-invalid-utf8";
    let dlq_topic = format!("{}-dlq", topic);
    let channel = "test-channel";
    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    let dlq_producer = Arc::new(plain_producer(&brokers));

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    // Produce a payload that is not valid UTF-8 (e.g. misrouted binary/Avro)
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();

    let invalid_utf8: &[u8] = &[0xFF, 0xFE, 0xFD, 0x00, 0x80];
    producer
        .send(
            FutureRecord::<str, [u8]>::to(topic).payload(invalid_utf8),
            Duration::from_secs(10),
        )
        .await
        .expect("Failed to produce invalid UTF-8 message");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let dlq_payload = wait_for_dlq_message(&brokers, &dlq_topic, 20)
        .await
        .expect("DLQ message not received within deadline");

    assert_eq!(dlq_payload["source_topic"], topic);
    assert!(dlq_payload["error"].as_str().unwrap().contains("UTF-8"));
    // Raw bytes are lossy-decoded into the envelope: invalid bytes -> U+FFFD
    assert!(
        dlq_payload["original_payload"]
            .as_str()
            .unwrap()
            .contains('\u{FFFD}')
    );

    handle.shutdown().await;
}

/// A message that fails without a confirmed DLQ write must not have its
/// offset committed: after a consumer restart it must be redelivered.
///
/// Phase 1 runs the consumer with the DLQ disabled, so the unparseable
/// message can only fail (retried in place, offset never committed).
/// Phase 2 restarts the consumer in the same group with the DLQ enabled;
/// the message is redelivered — proving the offset never advanced — and
/// this time lands in the DLQ.
#[tokio::test]
#[ignore]
async fn test_failed_message_redelivered_after_restart() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-redelivery";
    let dlq_topic = format!("{}-dlq", topic);
    let channel = "test-channel";
    let group_id = format!("test-redelivery-{}", uuid::Uuid::new_v4());
    let config = KafkaIngestConfig {
        group_id: group_id.clone(),
        dlq: DlqConfig {
            enabled: false,
            topic: dlq_topic.clone(),
        },
        ..test_kafka_config(&brokers, topic, channel)
    };
    let engine = empty_engine();

    // Phase 1: no DLQ producer — the message cannot be dead-lettered
    let handle = consumer::start_consumer(
        &config,
        engine.clone(),
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    producer
        .send(
            FutureRecord::<str, str>::to(topic).payload("unparseable {{{{"),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce test message");

    // Let the consumer receive the message and enter the retry loop
    // (backoff cycles at 1s/2s/4s — several failed attempts fit in 6s)
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Shutdown mid-retry: the offset must be left uncommitted
    handle.shutdown().await;

    // Phase 2: same group, DLQ enabled — the uncommitted message must be
    // redelivered and dead-lettered this time
    let config2 = KafkaIngestConfig {
        dlq: DlqConfig {
            enabled: true,
            topic: dlq_topic.clone(),
        },
        ..config.clone()
    };
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle2 = consumer::start_consumer(
        &config2,
        engine,
        test_registry(),
        test_datalogic(),
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    let dlq_payload = wait_for_dlq_message(&brokers, &dlq_topic, 30)
        .await
        .expect("message was not redelivered to the restarted consumer");

    assert_eq!(dlq_payload["source_topic"], topic);
    assert_eq!(dlq_payload["original_payload"], "unparseable {{{{");

    handle2.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn test_consumer_metadata_injection() {
    // Proves the consumer injects Kafka metadata (kafka_key, kafka_topic)
    // into the dispatched message: the target channel's validation_logic
    // passes ONLY when those metadata fields carry the produced values, and
    // the offset is committed only after validation + processing succeed.
    // If injection regressed, validation rejects every delivery and the
    // commit never happens.
    let (_container, brokers) = start_kafka().await;

    let topic = "test-metadata";
    let channel = "kafka-meta-ch";

    // Full AppState so the channel registry (which the consumer's guards
    // consult) is populated through the real admin API. The global Kafka
    // producer points at a dead address — only the consumer talks to the
    // container.
    let state =
        crate::common::test_state_with_kafka(orion::config::AppConfig::default(), "127.0.0.1:1")
            .await;
    let engine = state.engine.clone();
    let registry = state.channel_registry.clone();
    let datalogic = state.datalogic.clone();
    let app = orion::server::build_router(state);

    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        crate::common::simple_log_workflow("Kafka Metadata"),
        serde_json::json!({
            "validation_logic": { "and": [
                { "==": [ { "var": "metadata.kafka_key" }, "order-123" ] },
                { "==": [ { "var": "metadata.kafka_topic" }, topic ] }
            ]}
        }),
    )
    .await;

    let config = test_kafka_config(&brokers, topic, channel);
    let handle =
        consumer::start_consumer(&config, engine, registry, datalogic, None, None, None).unwrap();

    // Produce a message with the key the validation logic demands.
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

    wait_for_committed(&brokers, &config.group_id, topic, 1, 1, 30).await;
    shutdown_within(handle, 10).await;
}

// ============================================================
// Throughput tests (K4: processing is strictly sequential per consumer)
// ============================================================

/// Produce many messages concurrently and verify the consumer works through
/// all of them sequentially without skipping or wedging.
#[tokio::test]
#[ignore]
async fn test_concurrent_producers_all_committed() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-concurrent";
    let channel = "concurrent-channel";
    let msg_count = 50;

    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

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

    // All 50 must be processed to commit: the sequential loop has to keep
    // advancing (no wedge on any message) and no message may be skipped for
    // the committed total to reach 50.
    wait_for_committed(&brokers, &config.group_id, topic, 1, msg_count, 60).await;

    shutdown_within(handle, 10).await;
}

/// Produce a rapid burst of messages and verify the sequential consumer
/// keeps up.
#[tokio::test]
#[ignore]
async fn test_rapid_burst_all_committed() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-burst";
    let channel = "burst-channel";

    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

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

    // The consumer takes one message at a time; all 20 reaching the
    // committed offset proves the sequential loop keeps cycling under a
    // rapid burst instead of wedging or dropping.
    wait_for_committed(&brokers, &config.group_id, topic, 1, 20, 60).await;

    shutdown_within(handle, 10).await;
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
        lag_poll_interval_secs: 0,
        ..Default::default()
    };
    let engine = empty_engine();

    // Produce to both topics BEFORE starting the consumer. Subscribing to
    // topics that do not exist yet hits librdkafka's metadata-refresh
    // behaviour: unknown subscribed topics get a short burst of fast
    // refreshes, after which discovery falls back to
    // `topic.metadata.refresh.interval.ms` (5 minutes) — a topic created
    // after that burst is not consumed from for minutes. (The pre-rewrite
    // version of this test slept 5s and asserted nothing, which hid exactly
    // that.) Producing first creates both topics; `auto.offset.reset =
    // earliest` then delivers both messages to the late-starting group.
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .unwrap();

    producer
        .send(
            FutureRecord::<str, str>::to(topic_a).payload(r#"{"data": {"from": "a"}}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce to topic A");
    producer
        .send(
            FutureRecord::<str, str>::to(topic_b).payload(r#"{"data": {"from": "b"}}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce to topic B");

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    // One consumer, two topic mappings: the message on each topic must be
    // processed to commit under the same group.
    wait_for_committed(&brokers, &config.group_id, topic_a, 1, 1, 60).await;
    wait_for_committed(&brokers, &config.group_id, topic_b, 1, 1, 60).await;

    shutdown_within(handle, 10).await;
}

// ============================================================
// Partition rebalancing tests
// ============================================================

/// Start two consumers in the same group on a multi-partition topic.
/// Verify that both consumers process messages and no messages are lost
/// after rebalancing.
#[tokio::test]
#[ignore]
async fn test_consumer_partition_rebalance() {
    let (_container, brokers) = start_kafka().await;

    let topic = format!("test-rebalance-{}", uuid::Uuid::new_v4());
    let group_id = format!("rebalance-group-{}", uuid::Uuid::new_v4());

    // Create a topic with 3 partitions using the admin API
    let admin: rdkafka::admin::AdminClient<rdkafka::client::DefaultClientContext> =
        ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .create()
            .unwrap();

    use rdkafka::admin::{AdminOptions, NewTopic, TopicReplication};
    let new_topic = NewTopic::new(&topic, 3, TopicReplication::Fixed(1));
    let results = admin
        .create_topics(&[new_topic], &AdminOptions::new())
        .await
        .unwrap();
    for result in &results {
        assert!(result.is_ok(), "Failed to create topic: {:?}", result);
    }

    // Wait for topic to be fully created
    tokio::time::sleep(Duration::from_secs(2)).await;

    let engine = empty_engine();

    // Start consumer A
    let config_a = KafkaIngestConfig {
        enabled: true,
        brokers: vec![brokers.clone()],
        group_id: group_id.clone(),
        topics: vec![TopicMapping {
            topic: topic.clone(),
            channel: "rebalance-channel".to_string(),
        }],
        dlq: DlqConfig {
            enabled: false,
            topic: "unused".to_string(),
        },
        processing_timeout_ms: 5_000,
        lag_poll_interval_secs: 0,
        ..Default::default()
    };
    let handle_a = consumer::start_consumer(
        &config_a,
        engine.clone(),
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    // Produce initial batch of messages across partitions
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();

    for i in 0..9 {
        let payload = format!(r#"{{"data": {{"batch": 1, "index": {}}}}}"#, i);
        producer
            .send(
                FutureRecord::<str, str>::to(&topic)
                    .key(&format!("key-{}", i % 3)) // distribute across 3 partitions
                    .payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce message");
    }

    // All of batch 1 must be processed to commit before the rebalance.
    wait_for_committed(&brokers, &group_id, &topic, 3, 9, 60).await;

    // Start consumer B in the same group — triggers rebalance
    let config_b = KafkaIngestConfig {
        group_id: group_id.clone(),
        ..config_a.clone()
    };
    let handle_b = consumer::start_consumer(
        &config_b,
        engine.clone(),
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    // Wait for rebalance to complete
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Produce more messages — both consumers should process them
    for i in 0..9 {
        let payload = format!(r#"{{"data": {{"batch": 2, "index": {}}}}}"#, i);
        producer
            .send(
                FutureRecord::<str, str>::to(&topic)
                    .key(&format!("key-{}", i % 3))
                    .payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce message");
    }

    // Batch 2 must be fully processed across the rebalanced group — 18
    // committed in total proves no message was lost while partitions moved.
    wait_for_committed(&brokers, &group_id, &topic, 3, 18, 60).await;

    // Shutdown consumer B — triggers another rebalance, A picks up all partitions
    shutdown_within(handle_b, 10).await;

    // Produce final batch — only A remains to process it
    for i in 0..3 {
        let payload = format!(r#"{{"data": {{"batch": 3, "index": {}}}}}"#, i);
        producer
            .send(
                FutureRecord::<str, str>::to(&topic)
                    .key(&format!("key-{}", i))
                    .payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce message");
    }

    // 21 committed = every message across all three batches processed
    // through two rebalances with none lost.
    wait_for_committed(&brokers, &group_id, &topic, 3, 21, 60).await;

    shutdown_within(handle_a, 10).await;
}

// ============================================================
// Broker failure / recovery tests
// ============================================================

/// Freeze and thaw the Kafka broker. Verify the consumer survives the outage
/// and resumes processing — proven by the committed offset advancing past
/// messages produced after recovery.
///
/// `pause()`/`unpause()` rather than `stop()`/`start()`: a container restart
/// remaps the host port, so the original broker address goes stale and the
/// "reconnect" would target a dead port (the cluster tests avoid restarts
/// for the same reason). Pausing freezes the broker process while keeping
/// the port mapping, which is what a real broker outage looks like to a
/// connected client.
#[tokio::test]
#[ignore]
async fn test_consumer_broker_disconnect_recovery() {
    let (container, brokers) = start_kafka().await;

    let topic = "test-broker-recovery";
    let channel = "recovery-channel";
    let config = test_kafka_config(&brokers, topic, channel);
    let engine = empty_engine();

    let handle = consumer::start_consumer(
        &config,
        engine,
        test_registry(),
        test_datalogic(),
        None,
        None,
        None,
    )
    .unwrap();

    // Produce initial messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();

    for i in 0..5 {
        let payload = format!(r#"{{"data": {{"pre_outage": {}}}}}"#, i);
        producer
            .send(
                FutureRecord::<str, str>::to(topic).payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce message");
    }

    // All pre-outage messages processed and committed before the freeze.
    wait_for_committed(&brokers, &config.group_id, topic, 1, 5, 30).await;

    container.pause().await.unwrap();

    // Broker frozen: the consumer sees transport failures. Nothing to
    // assert mid-outage — the recovery assertions below fail if the
    // consumer wedged or died here.
    tokio::time::sleep(Duration::from_secs(5)).await;

    container.unpause().await.unwrap();

    // Produce after recovery with a retry loop: the broker may take a few
    // seconds to serve requests again after unpause.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut delivered = 0;
    while delivered < 5 {
        let payload = format!(r#"{{"data": {{"post_recovery": {}}}}}"#, delivered);
        match producer
            .send(
                FutureRecord::<str, str>::to(topic).payload(&payload),
                Duration::from_secs(10),
            )
            .await
        {
            Ok(_) => delivered += 1,
            Err((e, _)) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "producer could not reach the broker after unpause: {e}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    // The recovery contract: the same consumer instance processes the
    // post-outage messages to commit (5 pre + 5 post).
    wait_for_committed(&brokers, &config.group_id, topic, 1, 10, 60).await;

    shutdown_within(handle, 10).await;
}

// ============================================================
// publish_kafka through a workflow (T3) + connector broker routing (F13)
// ============================================================

/// T3: `publish_kafka` had no test at any level — connector resolution, key
/// and value JSONLogic, and the error envelope were all unexercised. F13:
/// the handler used to resolve the connector and throw it away, publishing
/// to the globally configured cluster no matter which connector was named.
#[tokio::test]
#[ignore]
async fn publish_kafka_publishes_to_the_connectors_brokers() {
    use tower::ServiceExt;

    let (_container, brokers) = start_kafka().await;
    let topic = "t3-publish-topic";

    // The global producer points at a broker that does not exist. If the
    // handler ignored the connector (F13), the publish would fail; the
    // message only lands because the connector's brokers are honoured.
    let state = crate::common::test_state_with_kafka(
        orion::config::AppConfig::default(),
        "127.0.0.1:1", // deliberately dead global cluster
    )
    .await;
    let app = orion::server::build_router(state);

    crate::common::create_connector(
        &app,
        serde_json::json!({
            "id": "t3-kafka",
            "name": "t3-kafka",
            "connector_type": "kafka",
            "config": {
                "type": "kafka",
                "brokers": [brokers.clone()],
                "topic": topic,
                // The broker container publishes on 127.0.0.1, which the S6
                // endpoint check refuses without this.
                "allow_private_urls": true
            }
        }),
    )
    .await;

    crate::common::create_and_activate_channel(
        &app,
        "t3-publish-ch",
        crate::common::workflow_with_tasks(
            "T3 Publish",
            serde_json::json!([
                {
                    // The request body lands in `payload`; copy it into
                    // `data` so the publish logic can address it.
                    "id": "t0",
                    "name": "Parse payload",
                    "function": {
                        "name": "parse_json",
                        "input": { "source": "payload", "target": "input" }
                    }
                },
                {
                    "id": "t1",
                    "name": "Publish",
                    "function": {
                        "name": "publish_kafka",
                        "input": {
                            "connector": "t3-kafka",
                            "topic": topic,
                            "key_logic": { "var": "data.input.order_id" },
                            "value_logic": { "var": "data.input" }
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(crate::common::json_request(
            "POST",
            "/api/v1/data/t3-publish-ch",
            Some(serde_json::json!({"data": {"order_id": "ORD-1", "amount": 42}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "publish must succeed against the connector's brokers: {:?}",
        crate::common::body_json(resp).await
    );

    // Consume it back to prove where it landed, and that key/value logic ran.
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("group.id", "t3-verify")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .unwrap();
    consumer.subscribe(&[topic]).unwrap();

    // Transient BrokerTransportFailure while the consumer discovers the
    // cluster is normal right after container start; retry until the
    // deadline rather than failing the assertion on it.
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let (key, payload) = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "message must arrive on the connector's cluster"
        );
        match tokio::time::timeout(Duration::from_secs(10), consumer.recv()).await {
            Ok(Ok(msg)) => {
                let key = msg.key().map(|k| String::from_utf8_lossy(k).to_string());
                let payload: serde_json::Value =
                    serde_json::from_slice(msg.payload().expect("payload")).unwrap();
                break (key, payload);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "consumer recv failed, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(_) => {}
        }
    };
    assert_eq!(key.as_deref(), Some("ORD-1"), "key_logic must be applied");
    assert_eq!(payload["order_id"], "ORD-1");
    assert_eq!(payload["amount"], 42);
}

/// T3: a workflow naming a non-Kafka connector must fail with a validation
/// error, not an opaque one. No container needed — the type check rejects
/// before any broker is contacted (rdkafka connects lazily), so this runs in
/// the default suite.
#[tokio::test]
async fn publish_kafka_rejects_a_non_kafka_connector() {
    use tower::ServiceExt;

    // Deliberately dead address (never :9092 — a dev box running a real
    // broker would let rdkafka's background thread actually connect).
    let state =
        crate::common::test_state_with_kafka(orion::config::AppConfig::default(), "127.0.0.1:1")
            .await;
    let app = orion::server::build_router(state);

    // Activate against a genuine Kafka connector — F52 refuses a `publish_kafka`
    // task pointing at anything else at activation time, so the runtime guard
    // this test covers is now only reachable when the connector changes *type*
    // under an already-active workflow.
    let conn_id = crate::common::create_connector(
        &app,
        serde_json::json!({
            "name": "t3-retyped",
            "connector_type": "kafka",
            "config": { "brokers": ["127.0.0.1:1"], "topic": "whatever" }
        }),
    )
    .await;
    crate::common::create_and_activate_channel(
        &app,
        "t3-wrong-conn-ch",
        crate::common::workflow_with_tasks(
            "T3 Wrong Connector",
            serde_json::json!([{
                "id": "t1",
                "name": "Publish",
                "continue_on_error": true,
                "function": {
                    "name": "publish_kafka",
                    "input": { "connector": "t3-retyped", "topic": "whatever" }
                }
            }]),
        ),
    )
    .await;

    // Retype it to `db`. The name is unchanged, so the F18 rename guard does
    // not apply — this is precisely the case the handler's own
    // `require_kafka_connector` check exists for.
    let resp = app
        .clone()
        .oneshot(crate::common::json_request(
            "PUT",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            Some(serde_json::json!({
                "connector_type": "db",
                "config": { "connection_string": "sqlite::memory:" }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(crate::common::json_request(
            "POST",
            "/api/v1/data/t3-wrong-conn-ch",
            Some(serde_json::json!({"data": {}})),
        ))
        .await
        .unwrap();
    let body = crate::common::body_json(resp).await;
    let errors = body["errors"].as_array().expect("task errors");
    assert!(!errors.is_empty(), "expected a task error, got {body}");
}

/// F52: the same mismatch is refused a layer earlier — a `publish_kafka` task
/// pointing at a `db` connector cannot be activated at all, so it never reaches
/// the runtime guard above.
#[tokio::test]
async fn publish_kafka_with_a_non_kafka_connector_cannot_activate() {
    use tower::ServiceExt;

    let app = crate::common::test_app().await;
    crate::common::create_connector(&app, crate::common::db_connector("t3-not-kafka")).await;

    let resp = app
        .clone()
        .oneshot(crate::common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(serde_json::json!({
                "name": "kafka-wrong-type",
                "tasks": [{
                    "id": "t1",
                    "name": "Publish",
                    "function": {
                        "name": "publish_kafka",
                        "input": { "connector": "t3-not-kafka", "topic": "whatever" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let wf_id = crate::common::body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(crate::common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(serde_json::json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    let msg = crate::common::body_json(resp).await.to_string();
    assert!(
        msg.contains("publish_kafka"),
        "must name the function: {msg}"
    );
    assert!(
        msg.contains("'kafka'"),
        "must say what type was required: {msg}"
    );
}

// ============================================================
// N16: the Kafka ingress runs the channel's guards
// ============================================================
//
// The unit tests in `channel::guards` and `kafka::consumer::process` pin the
// guard matrix and the refusal→commit translation as pure functions. What
// only a broker can show is what that translation does to an *offset*: a
// refusal that leaves the offset uncommitted has to end with the record
// processed, and one that commits must not also dead-letter. So these tests
// assert on committed offsets and DLQ contents, which is where a lost message
// shows up.
//
// Each one is written to fail against the code it guards: deleting the
// `apply_guards` call from `process_one_kafka_message` (or restoring the
// pre-N16 `validate_input`-only call) breaks all five, because without a
// guard every record simply commits.

/// Build a test config whose channel is guarded, with the DLQ toggled.
fn guarded_kafka_config(
    brokers: &str,
    topic: &str,
    channel: &str,
    group_id: &str,
    dlq_enabled: bool,
) -> KafkaIngestConfig {
    KafkaIngestConfig {
        group_id: group_id.to_string(),
        dlq: DlqConfig {
            enabled: dlq_enabled,
            topic: format!("{topic}-dlq"),
        },
        ..test_kafka_config(brokers, topic, channel)
    }
}

/// Produce one keyed record.
async fn produce(brokers: &str, topic: &str, key: &str, payload: &str) {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("message.timeout.ms", "10000")
        .create()
        .unwrap();
    producer
        .send(
            FutureRecord::to(topic).key(key).payload(payload),
            Duration::from_secs(10),
        )
        .await
        .expect("produce failed");
}

/// Count the DLQ envelopes that land on `dlq_topic` within `window_secs`.
///
/// Unlike [`wait_for_dlq_message`] this always waits the whole window: the
/// assertions below are about a record *not* being dead-lettered, and an
/// early return could not tell "none yet" from "none at all".
async fn count_dlq_messages(brokers: &str, dlq_topic: &str, window_secs: u64) -> usize {
    let dlq_consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", format!("dlq-counter-{}", uuid::Uuid::new_v4()))
        .set("auto.offset.reset", "earliest")
        .set("fetch.wait.max.ms", "500")
        .create()
        .unwrap();
    dlq_consumer.subscribe(&[dlq_topic]).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(window_secs);
    let mut count = 0;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), dlq_consumer.recv()).await {
            Ok(Ok(_)) => count += 1,
            Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(_) => {}
        }
    }
    count
}

/// Poll for `secs` and fail the moment any offset is committed.
///
/// The positive form of this — "nothing was committed" after a single sleep —
/// would pass against a consumer that committed and then crashed, so the
/// check runs continuously. A broker read error yields `-1`; that is a probe
/// failure, not a commit, so it is retried rather than asserted on.
async fn assert_no_commits_within(brokers: &str, group_id: &str, topic: &str, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        let committed = committed_total(brokers, group_id, topic, 1).await;
        assert!(
            committed <= 0,
            "group '{group_id}' committed {committed} offsets on '{topic}' — a record that \
             was never processed had its offset advanced, which is silent message loss"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Stand up a full `AppState` + router so channels can be created through the
/// real admin API, and hand back the pieces `start_consumer` needs. The
/// registry (and with it the channel's in-process dedup store) outlives any
/// number of consumers started from it, which is what lets a test restart the
/// consumer without resetting the guard state under test.
async fn guarded_consumer_app() -> (
    axum::Router,
    Arc<RwLock<Arc<dataflow_rs::Engine>>>,
    Arc<orion::channel::ChannelRegistry>,
    Arc<datalogic_rs::Engine>,
) {
    let state =
        crate::common::test_state_with_kafka(orion::config::AppConfig::default(), "127.0.0.1:1")
            .await;
    let engine = state.engine.clone();
    let registry = state.channel_registry.clone();
    let datalogic = state.datalogic.clone();
    (
        orion::server::build_router(state),
        engine,
        registry,
        datalogic,
    )
}

/// An HTTP connector pointing at a closed port, so every workflow that runs
/// fails and produces exactly one DLQ envelope. Counting envelopes is then a
/// count of records that actually reached the engine.
async fn dead_upstream_connector(app: &axum::Router, id: &str) {
    crate::common::create_connector(
        app,
        serde_json::json!({
            "id": id,
            "name": id,
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": "http://127.0.0.1:1",
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;
}

/// A one-task workflow whose only call is doomed.
fn doomed_workflow(name: &str, connector: &str) -> serde_json::Value {
    crate::common::workflow_with_tasks(
        name,
        serde_json::json!([{
            "id": "t1",
            "name": "Doomed call",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": connector,
                    "method": "POST",
                    "path": "/",
                    "body": {"x": 1},
                    "output": "data.response"
                }
            }
        }]),
    )
}

/// **The message-loss regression.** A guard claims the channel's idempotency
/// key before the workflow runs — that is the only moment the check is atomic
/// against a concurrent delivery. If the claim is never revisited, every
/// record the transport has to retry is destroyed: attempt 0 registers the
/// key, the attempt fails without committing its offset, and the retry reads
/// the key its own previous attempt wrote. `apply_guards` answers `409`, the
/// ingress reads that as "already handled", commits, and the record is gone —
/// never processed, never dead-lettered, and (per-message counters being
/// emitted on the first attempt only) not even counted.
///
/// Phase 1 runs a dedup-configured channel whose workflow always fails, with
/// the DLQ off, so no attempt may ever commit. Against the unfixed guard the
/// second attempt commits within a couple of seconds. Phase 2 restarts the
/// consumer against the *same* registry — so the same dedup store, with
/// whatever phase 1 left in it — with the DLQ on, and the record must be
/// redelivered and dead-lettered rather than silently skipped.
#[tokio::test]
#[ignore]
async fn a_deduplicated_record_survives_attempts_that_never_committed() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-dedup-retry";
    let dlq_topic = format!("{topic}-dlq");
    let channel = "n16-dedup-retry-ch";
    let group_id = format!("n16-dedup-retry-{}", uuid::Uuid::new_v4());

    let (app, engine, registry, datalogic) = guarded_consumer_app().await;
    dead_upstream_connector(&app, "n16-dead-retry").await;
    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        doomed_workflow("N16 Dedup Retry", "n16-dead-retry"),
        serde_json::json!({
            "deduplication": { "header": "idempotency-key", "window_secs": 300 }
        }),
    )
    .await;

    // Phase 1: no DLQ, so a failed record can only be retried in place.
    let config = guarded_kafka_config(&brokers, topic, channel, &group_id, false);
    let handle = consumer::start_consumer(
        &config,
        engine.clone(),
        registry.clone(),
        datalogic.clone(),
        None,
        None,
        None,
    )
    .unwrap();

    produce(&brokers, topic, "ORD-1", r#"{"data":{"id":1}}"#).await;

    // Several retry cycles (1s/2s/4s backoff) fit in this window, and every
    // one of them re-presents the same idempotency key.
    assert_no_commits_within(&brokers, &group_id, topic, 8).await;
    shutdown_within(handle, 10).await;

    // Phase 2: same registry, same dedup store, DLQ on. The record is
    // redelivered because its offset never advanced, and must be preserved.
    let config2 = guarded_kafka_config(&brokers, topic, channel, &group_id, true);
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle2 = consumer::start_consumer(
        &config2,
        engine,
        registry,
        datalogic,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    let dlq_payload = wait_for_dlq_message(&brokers, &dlq_topic, 45)
        .await
        .expect("the record must be redelivered, not skipped as a duplicate of itself");
    assert_eq!(dlq_payload["source_topic"], topic);
    wait_for_committed(&brokers, &group_id, topic, 1, 1, 30).await;
    shutdown_within(handle2, 10).await;
}

/// A record over the channel's rate limit is *throttled*, not discarded: the
/// offset is left uncommitted, the retry loop's capped backoff becomes the
/// throttle, and every record eventually lands. Dead-lettering a record for
/// arriving during a busy second would be data loss dressed as a policy, so
/// the DLQ must stay empty.
///
/// The elapsed-time floor is what makes this a test of the guard rather than
/// of the consumer: three records against a 1/s bucket cannot all commit in
/// under two seconds, and without the limit they commit in milliseconds.
#[tokio::test]
#[ignore]
async fn a_rate_limited_record_is_throttled_not_discarded() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-kafka-ratelimit";
    let dlq_topic = format!("{topic}-dlq");
    let channel = "n16-kafka-rl-ch";
    let group_id = format!("n16-kafka-rl-{}", uuid::Uuid::new_v4());

    let (app, engine, registry, datalogic) = guarded_consumer_app().await;
    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        crate::common::simple_log_workflow("N16 Kafka RL"),
        serde_json::json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;

    let config = guarded_kafka_config(&brokers, topic, channel, &group_id, true);
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle = consumer::start_consumer(
        &config,
        engine,
        registry,
        datalogic,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    for i in 0..3 {
        produce(
            &brokers,
            topic,
            &format!("ORD-{i}"),
            &format!(r#"{{"data":{{"id":{i}}}}}"#),
        )
        .await;
    }

    let started = tokio::time::Instant::now();
    wait_for_committed(&brokers, &group_id, topic, 1, 3, 90).await;
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "three records against a 1/s bucket cannot all commit at once — the channel's \
         rate limit is not being applied to the Kafka ingress"
    );
    assert_eq!(
        count_dlq_messages(&brokers, &dlq_topic, 5).await,
        0,
        "a rate-limited record must be retried, never dead-lettered"
    );
    shutdown_within(handle, 10).await;
}

/// A record that cannot get a backpressure permit is shed the way the
/// transport can absorb: the offset stays uncommitted so Kafka redelivers it,
/// and nothing is dead-lettered. `max_concurrent_per_node = 0` makes the
/// condition permanent, which is the only way to observe "shed" without
/// racing a release — and it is what distinguishes an applied permit from an
/// absent one, since without the guard the record commits immediately.
#[tokio::test]
#[ignore]
async fn a_record_shed_by_backpressure_is_neither_committed_nor_dead_lettered() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-kafka-backpressure";
    let dlq_topic = format!("{topic}-dlq");
    let channel = "n16-kafka-bp-ch";
    let group_id = format!("n16-kafka-bp-{}", uuid::Uuid::new_v4());

    let (app, engine, registry, datalogic) = guarded_consumer_app().await;
    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        crate::common::simple_log_workflow("N16 Kafka BP"),
        serde_json::json!({ "backpressure": { "max_concurrent_per_node": 0 } }),
    )
    .await;

    let config = guarded_kafka_config(&brokers, topic, channel, &group_id, true);
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle = consumer::start_consumer(
        &config,
        engine,
        registry,
        datalogic,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    produce(&brokers, topic, "ORD-BP", r#"{"data":{"id":1}}"#).await;

    assert_no_commits_within(&brokers, &group_id, topic, 8).await;
    assert_eq!(
        count_dlq_messages(&brokers, &dlq_topic, 5).await,
        0,
        "a shed record waits for capacity; dead-lettering it would discard traffic the \
         transport was willing to redeliver"
    );
    shutdown_within(handle, 10).await;
}

/// A record whose idempotency key was already *settled* by an earlier
/// delivery is skipped and its offset committed, with no DLQ write — nothing
/// failed. The doomed workflow is what makes this discriminating: every
/// record that actually reaches the engine produces exactly one DLQ envelope,
/// so "two offsets committed, one envelope" says the second record was
/// suppressed rather than run again.
#[tokio::test]
#[ignore]
async fn a_duplicate_record_commits_without_a_dlq_write() {
    let (_container, brokers) = start_kafka().await;

    let topic = "test-kafka-duplicate";
    let dlq_topic = format!("{topic}-dlq");
    let channel = "n16-kafka-dup-ch";
    let group_id = format!("n16-kafka-dup-{}", uuid::Uuid::new_v4());

    let (app, engine, registry, datalogic) = guarded_consumer_app().await;
    dead_upstream_connector(&app, "n16-dead-dup").await;
    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        doomed_workflow("N16 Kafka Dup", "n16-dead-dup"),
        serde_json::json!({
            "deduplication": { "header": "idempotency-key", "window_secs": 300 }
        }),
    )
    .await;

    let config = guarded_kafka_config(&brokers, topic, channel, &group_id, true);
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle = consumer::start_consumer(
        &config,
        engine,
        registry,
        datalogic,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    // The same record key twice — a producer retry, or a mirrored partition.
    produce(&brokers, topic, "ORD-DUP", r#"{"data":{"id":1}}"#).await;
    produce(&brokers, topic, "ORD-DUP", r#"{"data":{"id":1}}"#).await;

    wait_for_committed(&brokers, &group_id, topic, 1, 2, 60).await;
    assert_eq!(
        count_dlq_messages(&brokers, &dlq_topic, 8).await,
        1,
        "the duplicate must be skipped, not run again and dead-lettered a second time"
    );
    shutdown_within(handle, 10).await;
}

/// The channel's `timeout_ms` governs the Kafka dispatch, and the DLQ reports
/// the value the channel declared — not `kafka.processing_timeout_ms`, which
/// is 5 s here against a 1.5 s upstream, so nothing would be dead-lettered at
/// all if the channel value were ignored.
///
/// Only the shortening direction is observable here. The clamp in the other
/// direction is unit-tested in `channel::guards`: a channel asking for longer
/// than `kafka.processing_timeout_ms` would block this poll loop past
/// `max.poll.interval.ms`, and there is nothing to observe afterwards but an
/// evicted consumer.
#[tokio::test]
#[ignore]
async fn a_channel_timeout_shorter_than_the_kafka_ceiling_is_what_the_dlq_reports() {
    let (_container, brokers) = start_kafka().await;

    // An upstream slower than the channel's deadline but well inside the
    // transport's, so only the channel value can stop it.
    let mock_app = axum::Router::new().route(
        "/slow",
        axum::routing::post(|| async {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            axum::Json(serde_json::json!({"result": "done"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let topic = "test-kafka-timeout";
    let dlq_topic = format!("{topic}-dlq");
    let channel = "n16-kafka-timeout-ch";
    let group_id = format!("n16-kafka-timeout-{}", uuid::Uuid::new_v4());

    let (app, engine, registry, datalogic) = guarded_consumer_app().await;
    crate::common::create_connector(
        &app,
        serde_json::json!({
            "id": "n16-slow-api-kafka",
            "name": "n16-slow-api-kafka",
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{mock_addr}"),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;
    crate::common::create_and_activate_channel_with_config(
        &app,
        channel,
        crate::common::workflow_with_tasks(
            "N16 Kafka Timeout",
            serde_json::json!([{
                "id": "t1",
                "name": "Slow call",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "n16-slow-api-kafka",
                        "method": "POST",
                        "path": "/slow",
                        "body": {"x": 1},
                        "output": "data.response",
                        "timeout_ms": 5000
                    }
                }
            }]),
        ),
        serde_json::json!({ "timeout_ms": 100 }),
    )
    .await;

    // `test_kafka_config` sets `processing_timeout_ms: 5_000`; the channel
    // asks for 100 ms, and the upstream takes 1.5 s.
    let config = guarded_kafka_config(&brokers, topic, channel, &group_id, true);
    assert_eq!(config.processing_timeout_ms, 5_000);
    let dlq_producer = Arc::new(plain_producer(&brokers));
    let handle = consumer::start_consumer(
        &config,
        engine,
        registry,
        datalogic,
        Some(dlq_producer),
        Some(dlq_topic.clone()),
        None,
    )
    .unwrap();

    produce(&brokers, topic, "ORD-SLOW", r#"{"data":{"id":1}}"#).await;

    let dlq_payload = wait_for_dlq_message(&brokers, &dlq_topic, 45)
        .await
        .expect("the channel's 100ms deadline must stop a 1.5s workflow on the Kafka path too");
    let error = dlq_payload["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("100ms"),
        "the DLQ must report the channel's deadline, not the transport's: {error}"
    );
    assert!(
        !error.contains("5000ms"),
        "kafka.processing_timeout_ms must not be what stopped it: {error}"
    );
    shutdown_within(handle, 10).await;
}
