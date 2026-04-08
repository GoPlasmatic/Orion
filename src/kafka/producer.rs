use std::time::Duration;

use rdkafka::ClientConfig;
use rdkafka::message::{Header, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::errors::OrionError;

/// Thread-safe shared Kafka producer wrapping rdkafka's FutureProducer.
pub struct KafkaProducer {
    producer: FutureProducer,
}

impl KafkaProducer {
    /// Create a new producer connected to the given broker list.
    pub fn new(brokers: &str) -> Result<Self, OrionError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "30000")
            .set("acks", "all")
            .set("compression.type", "lz4")
            .set("linger.ms", "5")
            .set("batch.size", "65536")
            .create()
            .map_err(|e| OrionError::InternalSource {
                context: "Failed to create Kafka producer".to_string(),
                source: Box::new(e),
            })?;

        Ok(Self { producer })
    }

    /// Send a message to a Kafka topic with optional key.
    ///
    /// Waits for delivery confirmation with a 30-second timeout.
    /// Automatically injects W3C trace context headers when the `otel` feature is enabled.
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
                context: format!("Kafka send to '{}' failed", topic),
                source: Box::new(e),
            })?;

        Ok(())
    }
}
