use std::time::Duration;

use rdkafka::ClientConfig;
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
            .create()
            .map_err(|e| OrionError::Internal(format!("Failed to create Kafka producer: {}", e)))?;

        Ok(Self { producer })
    }

    /// Send a message to a Kafka topic with optional key.
    ///
    /// Waits for delivery confirmation with a 30-second timeout.
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

        self.producer
            .send(record, Duration::from_secs(30))
            .await
            .map_err(|(e, _)| {
                OrionError::Internal(format!("Kafka send to '{}' failed: {}", topic, e))
            })?;

        Ok(())
    }
}
