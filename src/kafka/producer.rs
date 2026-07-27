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
