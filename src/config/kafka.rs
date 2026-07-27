use std::collections::HashMap;

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
    /// Authentication / TLS settings applied to every Kafka client
    /// (consumer, producer, DLQ producer).
    pub auth: KafkaAuthConfig,
    /// Raw librdkafka properties applied to every Kafka client after all
    /// Orion-managed settings, so entries here override anything —
    /// including `[kafka.auth]`. Free-form maps do not fit the
    /// `ORION_SECTION__KEY` env-override scheme, so there is no
    /// `ORION_KAFKA__EXTRA_CONFIG`; set this via TOML only.
    pub extra_config: HashMap<String, String>,
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
            auth: KafkaAuthConfig::default(),
            extra_config: HashMap::new(),
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
        self.auth.validate()?;
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

/// Accepted values for `kafka.auth.security_protocol` (librdkafka
/// `security.protocol`).
const VALID_SECURITY_PROTOCOLS: [&str; 4] = ["plaintext", "ssl", "sasl_plaintext", "sasl_ssl"];

/// Accepted values for `kafka.auth.sasl_mechanism` (librdkafka
/// `sasl.mechanism`). GSSAPI/OAUTHBEARER are not supported — librdkafka is
/// built without libsasl2.
const VALID_SASL_MECHANISMS: [&str; 3] = ["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"];

/// Authentication / encryption settings shared by every Kafka client Orion
/// creates (ingest consumer, `publish_kafka` producer, DLQ producer). Each
/// field maps 1:1 onto a librdkafka property; unset fields leave librdkafka
/// defaults (plaintext, no auth) untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KafkaAuthConfig {
    /// librdkafka `security.protocol`: `plaintext`, `ssl`, `sasl_plaintext`,
    /// or `sasl_ssl` (case-insensitive).
    pub security_protocol: Option<String>,
    /// librdkafka `sasl.mechanism`: `PLAIN`, `SCRAM-SHA-256`, or
    /// `SCRAM-SHA-512` (case-insensitive).
    pub sasl_mechanism: Option<String>,
    /// librdkafka `sasl.username`.
    pub sasl_username: Option<String>,
    /// librdkafka `sasl.password`.
    pub sasl_password: Option<String>,
    /// librdkafka `ssl.ca.location` — CA certificate bundle path for broker
    /// certificate verification. Unset uses the bundled/system trust store.
    pub ssl_ca_location: Option<String>,
}

impl KafkaAuthConfig {
    fn validate(&self) -> Result<(), OrionError> {
        if let Some(protocol) = &self.security_protocol {
            let normalized = protocol.to_lowercase();
            if !VALID_SECURITY_PROTOCOLS.contains(&normalized.as_str()) {
                return Err(OrionError::Config {
                    message: format!(
                        "kafka.auth.security_protocol '{protocol}' is invalid, expected one of: {}",
                        VALID_SECURITY_PROTOCOLS.join(", ")
                    ),
                });
            }
            if normalized.starts_with("sasl")
                && (self.sasl_mechanism.is_none()
                    || self.sasl_username.is_none()
                    || self.sasl_password.is_none())
            {
                return Err(OrionError::Config {
                    message: format!(
                        "kafka.auth.security_protocol '{protocol}' requires sasl_mechanism, \
                         sasl_username, and sasl_password to be set"
                    ),
                });
            }
        }
        if let Some(mechanism) = &self.sasl_mechanism {
            let normalized = mechanism.to_uppercase();
            if !VALID_SASL_MECHANISMS.contains(&normalized.as_str()) {
                return Err(OrionError::Config {
                    message: format!(
                        "kafka.auth.sasl_mechanism '{mechanism}' is invalid, expected one of: {}",
                        VALID_SASL_MECHANISMS.join(", ")
                    ),
                });
            }
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config(auth: KafkaAuthConfig) -> KafkaIngestConfig {
        KafkaIngestConfig {
            enabled: true,
            auth,
            ..KafkaIngestConfig::default()
        }
    }

    #[test]
    fn test_auth_default_is_valid() {
        assert!(
            enabled_config(KafkaAuthConfig::default())
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn test_auth_sasl_ssl_with_credentials_is_valid() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("SASL_SSL".to_string()),
            sasl_mechanism: Some("scram-sha-512".to_string()),
            sasl_username: Some("user".to_string()),
            sasl_password: Some("pass".to_string()),
            ssl_ca_location: None,
        };
        assert!(enabled_config(auth).validate().is_ok());
    }

    #[test]
    fn test_auth_sasl_protocol_requires_credentials() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("sasl_plaintext".to_string()),
            sasl_mechanism: Some("PLAIN".to_string()),
            sasl_username: Some("user".to_string()),
            sasl_password: None,
            ssl_ca_location: None,
        };
        let err = enabled_config(auth).validate().expect_err("test");
        assert!(err.to_string().contains("sasl_password"));
    }

    #[test]
    fn test_auth_invalid_security_protocol() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("kerberos".to_string()),
            ..KafkaAuthConfig::default()
        };
        let err = enabled_config(auth).validate().expect_err("test");
        assert!(err.to_string().contains("security_protocol"));
    }

    #[test]
    fn test_auth_invalid_sasl_mechanism() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("sasl_ssl".to_string()),
            sasl_mechanism: Some("GSSAPI".to_string()),
            sasl_username: Some("user".to_string()),
            sasl_password: Some("pass".to_string()),
            ssl_ca_location: None,
        };
        let err = enabled_config(auth).validate().expect_err("test");
        assert!(err.to_string().contains("sasl_mechanism"));
    }

    #[test]
    fn test_auth_not_validated_when_kafka_disabled() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("bogus".to_string()),
            ..KafkaAuthConfig::default()
        };
        let config = KafkaIngestConfig {
            enabled: false,
            auth,
            ..KafkaIngestConfig::default()
        };
        assert!(config.validate().is_ok());
    }
}
