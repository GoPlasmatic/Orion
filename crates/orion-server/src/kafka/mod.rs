pub mod consumer;
pub mod producer;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rdkafka::ClientConfig;

use crate::config::KafkaAuthConfig;

/// Health signal for the Kafka ingest consumer, shared between the reload
/// path, the restart supervisor and the readiness endpoints (K7/O10).
///
/// `degraded` means "ingestion should be running and is not": a consumer
/// (re)start failed and the supervisor has not brought one back yet. It is
/// deliberately *not* set when the merged topic list is empty — a consumer
/// with nothing to consume is idle, not broken.
#[derive(Default)]
pub struct KafkaIngestStatus {
    degraded: AtomicBool,
    /// Single-occupancy slot for the restart supervisor, so overlapping
    /// reload failures spawn one retry loop, not one each.
    supervisor_active: AtomicBool,
}

impl KafkaIngestStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    /// Flip the degraded signal, mirrored to the
    /// `orion_kafka_ingest_degraded` gauge so it can be alerted on.
    pub fn set_degraded(&self, degraded: bool) {
        self.degraded.store(degraded, Ordering::Release);
        crate::metrics::set_kafka_ingest_degraded(degraded);
    }

    /// Claim the restart-supervisor slot; `false` when one is already running.
    pub(crate) fn claim_supervisor(&self) -> bool {
        self.supervisor_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn release_supervisor(&self) {
        self.supervisor_active.store(false, Ordering::Release);
    }

    /// Whether the restart-supervisor slot is currently claimed. A read-only
    /// probe for tests and diagnostics; the slot itself is managed only by
    /// `claim_supervisor` / `release_supervisor`.
    pub fn supervisor_active(&self) -> bool {
        self.supervisor_active.load(Ordering::Acquire)
    }
}

/// Apply `[kafka.auth]` and then `kafka.extra_config` to an rdkafka client
/// config. Shared by the ingest consumer and the producer so every Kafka
/// client authenticates identically. `extra_config` is applied last so its
/// entries override any Orion-managed property, including the auth keys.
pub(crate) fn apply_client_auth(
    client: &mut ClientConfig,
    auth: &KafkaAuthConfig,
    extra_config: &HashMap<String, String>,
) {
    if let Some(protocol) = &auth.security_protocol {
        client.set("security.protocol", protocol.to_lowercase());
    }
    if let Some(mechanism) = &auth.sasl_mechanism {
        client.set("sasl.mechanism", mechanism.to_uppercase());
    }
    if let Some(username) = &auth.sasl_username {
        client.set("sasl.username", username);
    }
    if let Some(password) = &auth.sasl_password {
        client.set("sasl.password", password);
    }
    if let Some(ca_location) = &auth.ssl_ca_location {
        client.set("ssl.ca.location", ca_location);
    }
    for (key, value) in extra_config {
        client.set(key, value);
    }
}

/// Merge the config-file topic mappings with the DB-driven ones: every active
/// Kafka-protocol or async channel that names a `topic` gets a mapping, unless
/// a config-file entry already claims that topic. Startup and engine reload
/// both consume exactly this list, so the consumer subscribes identically on
/// both paths.
pub fn merge_kafka_topics(
    config: &crate::config::KafkaIngestConfig,
    channels: &[crate::storage::models::Channel],
) -> Vec<crate::config::TopicMapping> {
    let mut all_topics = config.topics.clone();
    for ch in channels {
        // A cron channel is `channel_type: "async"`, which is the other half of
        // the condition below — so it has to be excluded explicitly or a stored
        // row carrying a stray `topic` would be subscribed as a Kafka consumer
        // *and* scheduled. Validation refuses `topic` on this protocol, so the
        // row cannot be written through the API; this is what makes the rule
        // hold for one written any other way.
        if ch.protocol == crate::storage::models::ChannelProtocol::Cron.as_str() {
            continue;
        }
        if (ch.protocol == crate::storage::models::ChannelProtocol::Kafka.as_str()
            || ch.channel_type == "async")
            && let Some(ref topic) = ch.topic
            && !all_topics.iter().any(|t| t.topic == *topic)
        {
            all_topics.push(crate::config::TopicMapping {
                topic: topic.clone(),
                channel: ch.name.clone(),
            });
        }
    }
    all_topics
}

/// Probe broker reachability: connect with the configured brokers + auth and
/// fetch cluster metadata. Returns the number of brokers visible. Blocking
/// (librdkafka metadata fetch) — used by the `test-connectivity` CLI
/// subcommand, where blocking for the timeout is fine.
pub fn probe_brokers(
    config: &crate::config::KafkaIngestConfig,
    timeout: std::time::Duration,
) -> Result<usize, String> {
    use rdkafka::consumer::{BaseConsumer, Consumer};

    let mut client = ClientConfig::new();
    client.set("bootstrap.servers", config.brokers.join(","));
    apply_client_auth(&mut client, &config.auth, &config.extra_config);
    let consumer: BaseConsumer = client
        .create()
        .map_err(|e| format!("client construction failed: {e}"))?;
    let metadata = consumer
        .fetch_metadata(None, timeout)
        .map_err(|e| format!("metadata fetch failed: {e}"))?;
    Ok(metadata.brokers().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTH_KEYS: [&str; 5] = [
        "security.protocol",
        "sasl.mechanism",
        "sasl.username",
        "sasl.password",
        "ssl.ca.location",
    ];

    /// A cron channel subscribes to nothing, even though it shares
    /// `channel_type: "async"` with the channels that do.
    #[test]
    fn a_cron_channel_contributes_no_kafka_topic() {
        let now = chrono::Utc::now().naive_utc();
        let channel = crate::storage::models::Channel {
            tags_json: "[]".to_string(),
            channel_id: "ch_nightly".to_string(),
            version: 1,
            name: "nightly".to_string(),
            description: None,
            channel_type: "async".to_string(),
            protocol: "cron".to_string(),
            methods_json: None,
            route_pattern: None,
            // Not reachable through the API — validation refuses it — which is
            // exactly why the exclusion must not depend on it being absent.
            topic: Some("orders".to_string()),
            consumer_group: None,
            transport_config_json: r#"{"schedule": "0 15 2 * * *"}"#.to_string(),
            workflow_id: Some("wf".to_string()),
            config_json: "{}".to_string(),
            status: "active".to_string(),
            priority: 0,
            created_at: now,
            updated_at: now,
        };
        let merged = merge_kafka_topics(
            &crate::config::KafkaIngestConfig::default(),
            std::slice::from_ref(&channel),
        );
        assert!(
            merged.is_empty(),
            "a cron channel must not be subscribed: {merged:?}"
        );

        // The same row as a plain async channel is subscribed, so the test is
        // pinning the protocol exclusion rather than an unrelated condition.
        let async_channel = crate::storage::models::Channel {
            protocol: "http".to_string(),
            ..channel
        };
        assert_eq!(
            merge_kafka_topics(
                &crate::config::KafkaIngestConfig::default(),
                &[async_channel]
            )
            .len(),
            1
        );
    }

    #[test]
    fn test_sasl_ssl_scram_sets_exact_keys() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("SASL_SSL".to_string()),
            sasl_mechanism: Some("scram-sha-256".to_string()),
            sasl_username: Some("orion-user".to_string()),
            sasl_password: Some("secret".to_string()),
            ssl_ca_location: Some("/etc/kafka/ca.pem".to_string()),
        };
        let mut client = ClientConfig::new();
        apply_client_auth(&mut client, &auth, &HashMap::new());

        assert_eq!(client.get("security.protocol"), Some("sasl_ssl"));
        assert_eq!(client.get("sasl.mechanism"), Some("SCRAM-SHA-256"));
        assert_eq!(client.get("sasl.username"), Some("orion-user"));
        assert_eq!(client.get("sasl.password"), Some("secret"));
        assert_eq!(client.get("ssl.ca.location"), Some("/etc/kafka/ca.pem"));
    }

    #[test]
    fn test_default_auth_sets_no_keys() {
        let mut client = ClientConfig::new();
        apply_client_auth(&mut client, &KafkaAuthConfig::default(), &HashMap::new());

        for key in AUTH_KEYS {
            assert_eq!(client.get(key), None, "expected '{key}' to be unset");
        }
    }

    #[test]
    fn test_extra_config_passthrough_and_precedence() {
        let auth = KafkaAuthConfig {
            security_protocol: Some("ssl".to_string()),
            ..KafkaAuthConfig::default()
        };
        let extra = HashMap::from([
            ("security.protocol".to_string(), "plaintext".to_string()),
            ("socket.timeout.ms".to_string(), "12345".to_string()),
        ]);
        let mut client = ClientConfig::new();
        apply_client_auth(&mut client, &auth, &extra);

        // extra_config wins over [kafka.auth] and passes unknown keys through
        assert_eq!(client.get("security.protocol"), Some("plaintext"));
        assert_eq!(client.get("socket.timeout.ms"), Some("12345"));
    }
}
