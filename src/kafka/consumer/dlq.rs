//! Failure reporting for the Kafka consumer: metric/log labels per failure
//! class and the dead-letter-queue write whose confirmation gates the
//! offset commit.

use std::sync::Arc;

use crate::kafka::producer::KafkaProducer;
use crate::metrics;

use super::ConsumeLoopContext;
use super::process::MsgOutcome;

/// Per-failure descriptor for [`report_failure_and_dlq`]. Bundles the
/// message identity (channel/topic/payload) with the metric and log
/// labels for one of the consume loop's failure branches.
pub(super) struct FailureReport<'a> {
    pub(super) channel: &'a str,
    pub(super) topic: &'a str,
    pub(super) payload: &'a [u8],
    pub(super) message_status: &'static str,
    pub(super) error_kind: &'static str,
    pub(super) log_msg: &'a str,
    pub(super) dlq_reason: &'a str,
}

/// Record failure metrics and ship the original payload to the DLQ.
/// Used by every failure branch in `process_one_kafka_message`
/// (unmapped topic, UTF-8 decode, empty payload, JSON parse, channel
/// validation, processing timeout, engine error, workflow errors) to keep
/// metric / log / DLQ / commit behaviour consistent. Returns [`MsgOutcome::DeadLettered`] only
/// when the DLQ write was confirmed, [`MsgOutcome::Failed`] otherwise.
pub(super) async fn report_failure_and_dlq(
    ctx: &ConsumeLoopContext,
    failure: FailureReport<'_>,
) -> MsgOutcome {
    metrics::record_message(failure.channel, failure.message_status);
    metrics::record_error(failure.error_kind);
    tracing::error!(
        topic = %failure.topic,
        channel = %failure.channel,
        error = %failure.dlq_reason,
        "{}",
        failure.log_msg
    );
    let dead_lettered = send_to_dlq(
        &ctx.dlq_producer,
        &ctx.dlq_topic,
        failure.topic,
        failure.payload,
        failure.dlq_reason,
    )
    .await;
    if dead_lettered {
        MsgOutcome::DeadLettered
    } else {
        MsgOutcome::Failed
    }
}

/// Build a DLQ envelope message from error context. The original payload
/// is embedded lossy-decoded (the envelope is JSON text; any invalid
/// UTF-8 bytes become U+FFFD replacement characters).
fn build_dlq_message(source_topic: &str, payload: &[u8], error: &str) -> serde_json::Value {
    serde_json::json!({
        "source_topic": source_topic,
        "error": error,
        "original_payload": String::from_utf8_lossy(payload),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// Send a failed message to the dead-letter queue if configured. Returns
/// `true` only when the DLQ producer confirmed delivery — the caller uses
/// this to decide whether the source offset may be committed.
async fn send_to_dlq(
    producer: &Option<Arc<KafkaProducer>>,
    dlq_topic: &Option<String>,
    source_topic: &str,
    payload: &[u8],
    error: &str,
) -> bool {
    let (Some(producer), Some(topic)) = (producer, dlq_topic) else {
        return false;
    };
    let dlq_message = build_dlq_message(source_topic, payload, error);

    // Infallible: the envelope is a `serde_json::Value` built from
    // string-keyed literals via `json!`, which cannot fail to serialise.
    let dlq_payload =
        serde_json::to_string(&dlq_message).expect("DLQ envelope is always serialisable");
    match producer
        .send(topic, Some(source_topic), dlq_payload.as_bytes())
        .await
    {
        Err(e) => {
            tracing::error!(
                dlq_topic = %topic,
                error = %e,
                "Failed to send message to DLQ"
            );
            false
        }
        Ok(()) => {
            tracing::debug!(
                dlq_topic = %topic,
                source_topic = %source_topic,
                "Message sent to DLQ"
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dlq_message_format() {
        let payload = br#"{"data": {"broken": true}}"#;
        let msg = build_dlq_message("test-topic", payload, "JSON parse error");

        assert_eq!(msg["source_topic"], "test-topic");
        assert_eq!(msg["error"], "JSON parse error");
        assert_eq!(msg["original_payload"], r#"{"data": {"broken": true}}"#);
        // Timestamp should be a valid RFC3339 string
        let ts = msg["timestamp"].as_str().expect("test");
        assert!(ts.contains("T"));
        assert!(ts.ends_with('Z') || ts.contains('+'));
    }

    #[test]
    fn test_dlq_message_invalid_utf8_payload() {
        let payload: &[u8] = &[0xFF, 0xFE, 0xFD];
        let msg = build_dlq_message("bad-topic", payload, "UTF-8 decode error");

        assert_eq!(msg["source_topic"], "bad-topic");
        assert_eq!(msg["error"], "UTF-8 decode error");
        // Lossy conversion should produce replacement characters
        let original = msg["original_payload"].as_str().expect("test");
        assert!(original.contains('\u{FFFD}'));
    }

    #[test]
    fn test_dlq_message_empty_payload() {
        let msg = build_dlq_message("topic", b"", "empty message");

        assert_eq!(msg["source_topic"], "topic");
        assert_eq!(msg["error"], "empty message");
        assert_eq!(msg["original_payload"], "");
    }
}
