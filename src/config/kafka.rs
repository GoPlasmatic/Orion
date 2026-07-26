use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KafkaIngestConfig {
    /// Enable Kafka consumer ingestion.
    pub enabled: bool,
    /// Kafka broker addresses.
    pub brokers: Vec<String>,
    /// Consumer group ID.
    pub group_id: String,
    /// Topic-to-channel mappings.
    #[serde(default)]
    pub topics: Vec<TopicMapping>,
    /// Dead-letter queue configuration.
    pub dlq: DlqConfig,
    /// Maximum time in milliseconds for processing a single Kafka message.
    pub processing_timeout_ms: u64,
    /// Maximum number of in-flight messages being processed concurrently.
    /// The consumer pauses reading when this limit is reached (backpressure).
    pub max_inflight: usize,
    /// Interval in seconds between consumer lag metric polls.
    /// Set to 0 to disable lag monitoring.
    pub lag_poll_interval_secs: u64,
    /// Kafka `session.timeout.ms`. Only applied in cluster mode, together
    /// with static group membership (`group.instance.id` = the instance id),
    /// so rolling restarts rejoin without a full group rebalance.
    pub session_timeout_ms: u64,
}

impl Default for KafkaIngestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            brokers: vec!["localhost:9092".to_string()],
            group_id: "orion".to_string(),
            topics: vec![],
            dlq: DlqConfig::default(),
            processing_timeout_ms: 60_000,
            max_inflight: 100,
            lag_poll_interval_secs: 30,
            session_timeout_ms: 45_000,
        }
    }
}

impl KafkaIngestConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if !self.enabled {
            return Ok(());
        }
        if self.brokers.is_empty() {
            return Err(OrionError::Config {
                message: "kafka.brokers must not be empty when Kafka is enabled".to_string(),
            });
        }
        if self.group_id.is_empty() {
            return Err(OrionError::Config {
                message: "kafka.group_id must not be empty when Kafka is enabled".to_string(),
            });
        }
        for (i, broker) in self.brokers.iter().enumerate() {
            let broker = broker.trim();
            if broker.is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.brokers[{i}] must not be empty"),
                });
            }
            if !broker.contains(':') {
                return Err(OrionError::Config {
                    message: format!("kafka.brokers[{i}] '{broker}' must be in host:port format"),
                });
            }
            let port_str = broker.rsplit(':').next().unwrap_or("");
            if port_str.parse::<u16>().is_err() {
                return Err(OrionError::Config {
                    message: format!("kafka.brokers[{i}] '{broker}' has invalid port"),
                });
            }
        }
        if self.max_inflight == 0 {
            return Err(OrionError::Config {
                message: "kafka.max_inflight must be > 0".to_string(),
            });
        }
        // Topics can be empty in config when async channels provide them from DB
        let mut seen_topics = std::collections::HashSet::new();
        let mut seen_channels = std::collections::HashSet::new();
        for (i, mapping) in self.topics.iter().enumerate() {
            if mapping.topic.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.topics[{i}].topic must not be empty"),
                });
            }
            if mapping.channel.trim().is_empty() {
                return Err(OrionError::Config {
                    message: format!("kafka.topics[{i}].channel must not be empty"),
                });
            }
            if !seen_topics.insert(&mapping.topic) {
                return Err(OrionError::Config {
                    message: format!("kafka.topics: duplicate topic '{}'", mapping.topic),
                });
            }
            if !seen_channels.insert(&mapping.channel) {
                return Err(OrionError::Config {
                    message: format!("kafka.topics: duplicate channel '{}'", mapping.channel),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMapping {
    pub topic: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DlqConfig {
    /// Enable dead-letter queue for failed messages.
    pub enabled: bool,
    /// DLQ topic name.
    pub topic: String,
}

impl Default for DlqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            topic: "orion-dlq".to_string(),
        }
    }
}
