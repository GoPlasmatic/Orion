use std::collections::HashMap;
use std::time::Duration;

use rdkafka::ClientConfig;
use rdkafka::message::{Header, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::config::KafkaAuthConfig;
use crate::errors::OrionError;

/// Thread-safe shared Kafka producer wrapping rdkafka's FutureProducer.
pub struct KafkaProducer {
    producer: FutureProducer,
}

/// Per-broker-set producer cache (F13).
///
/// `publish_kafka` resolved its connector and then published through the one
/// globally configured producer regardless — so workflows naming connectors
/// for different clusters all wrote to the same one. Producers are keyed by
/// the connector's broker list; the globally configured brokers resolve to
/// the shared producer rather than a second connection to the same cluster.
pub struct KafkaProducerCache {
    global_brokers: String,
    global: std::sync::Arc<KafkaProducer>,
    auth: KafkaAuthConfig,
    extra_config: HashMap<String, String>,
    by_brokers: tokio::sync::RwLock<HashMap<String, std::sync::Arc<KafkaProducer>>>,
}

impl KafkaProducerCache {
    pub fn new(
        global_brokers: String,
        global: std::sync::Arc<KafkaProducer>,
        auth: KafkaAuthConfig,
        extra_config: HashMap<String, String>,
    ) -> Self {
        Self {
            global_brokers: normalize_brokers(&global_brokers),
            global,
            auth,
            extra_config,
            by_brokers: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// The producer for `brokers`, creating and caching one on first use.
    /// An empty list means "use the globally configured cluster".
    pub async fn for_brokers(
        &self,
        brokers: &[String],
    ) -> Result<std::sync::Arc<KafkaProducer>, OrionError> {
        if brokers.is_empty() {
            return Ok(self.global.clone());
        }
        let key = normalize_brokers(&brokers.join(","));
        if key == self.global_brokers {
            return Ok(self.global.clone());
        }
        if let Some(p) = self.by_brokers.read().await.get(&key) {
            return Ok(p.clone());
        }
        let producer =
            std::sync::Arc::new(KafkaProducer::new(&key, &self.auth, &self.extra_config)?);
        let mut map = self.by_brokers.write().await;
        // Another task may have raced us here; prefer the stored one so a
        // single producer per cluster stays the invariant.
        Ok(map.entry(key).or_insert(producer).clone())
    }
}

/// Order- and whitespace-insensitive broker-list key, so `a,b` and `b, a`
/// share one producer.
fn normalize_brokers(brokers: &str) -> String {
    let mut parts: Vec<&str> = brokers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    parts.sort_unstable();
    parts.join(",")
}

impl KafkaProducer {
    /// Create a new producer connected to the given broker list, using the
    /// shared `[kafka.auth]` / `kafka.extra_config` client settings.
    pub fn new(
        brokers: &str,
        auth: &KafkaAuthConfig,
        extra_config: &HashMap<String, String>,
    ) -> Result<Self, OrionError> {
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "30000")
            // K6: `acks=all` alone prevents neither duplicates nor reordering.
            // librdkafka's default retry on a timed-out produce can duplicate a
            // record, and the default max.in.flight=5 can reorder within a
            // partition. Both matter because `publish_kafka` is a workflow side
            // effect and the DLQ producer shares this client — a duplicated DLQ
            // envelope is a duplicated failure record. Enabling idempotence
            // makes librdkafka enforce acks=all, max.in.flight<=5 and
            // retries=INT32_MAX itself.
            .set("enable.idempotence", "true")
            .set("delivery.timeout.ms", "120000")
            .set("acks", "all")
            .set("compression.type", "lz4")
            .set("linger.ms", "5")
            .set("batch.size", "65536");
        super::apply_client_auth(&mut client_config, auth, extra_config);

        let producer: FutureProducer =
            client_config
                .create()
                .map_err(|e| OrionError::InternalSource {
                    context: "Failed to create Kafka producer".to_string(),
                    source: Box::new(e),
                })?;

        Ok(Self { producer })
    }

    /// Send a message to a Kafka topic with optional key.
    ///
    /// Waits for delivery confirmation with a 30-second timeout. Always injects
    /// the current span's W3C trace context as Kafka message headers; receivers
    /// can stitch the trace if `tracing.enabled` is on at both ends.
    pub async fn send(
        &self,
        topic: &str,
        key: Option<&str>,
        payload: &[u8],
    ) -> Result<(), OrionError> {
        let mut record = FutureRecord::to(topic).payload(payload);

        if let Some(k) = key {
            record = record.key(k);
        }

        // Inject trace context as Kafka message headers
        let headers = {
            let mut trace_headers = std::collections::HashMap::new();
            crate::server::trace_context::inject_trace_context(&mut trace_headers);
            let mut kafka_headers = OwnedHeaders::new();
            for (k, v) in &trace_headers {
                kafka_headers = kafka_headers.insert(Header {
                    key: k,
                    value: Some(v.as_bytes()),
                });
            }
            kafka_headers
        };
        let record = record.headers(headers);

        self.producer
            .send(record, Duration::from_secs(30))
            .await
            .map_err(|(e, _)| OrionError::InternalSource {
                context: format!("Kafka send to '{topic}' failed"),
                source: Box::new(e),
            })?;

        Ok(())
    }
}
