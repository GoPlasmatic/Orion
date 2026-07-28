//! Per-message processing for the Kafka consumer: decode, parse, validate,
//! dispatch through the engine, and the in-place retry loop that decides
//! when an offset may be committed (see the module doc in `mod.rs` for the
//! at-least-once delivery guarantee).

use std::collections::HashMap;
use std::time::Instant;

use rdkafka::Message as _;
use rdkafka::consumer::Consumer;
use tokio::sync::watch;

use crate::metrics;

use super::ConsumeLoopContext;
use super::dlq::{FailureReport, report_failure_and_dlq};

/// Outcome of processing a single Kafka message, deciding whether its
/// offset may be committed (see the module doc for the delivery guarantee).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MsgOutcome {
    /// Processed successfully.
    Processed,
    /// Processing failed but the payload was confirmed written to the DLQ.
    DeadLettered,
    /// Processing failed and the payload is not preserved anywhere (DLQ
    /// disabled, or the DLQ write itself failed).
    Failed,
}

impl MsgOutcome {
    /// Whether the message's offset may be committed. Committing an offset
    /// implicitly commits every earlier offset on the partition, so this
    /// must be true only when the message no longer needs redelivery.
    fn commits_offset(self) -> bool {
        matches!(self, MsgOutcome::Processed | MsgOutcome::DeadLettered)
    }
}

/// Decode + parse + dispatch a single Kafka message. Wraps the entire
/// per-message lifecycle: topic → channel lookup, payload UTF-8 decode,
/// JSON parse, W3C trace context extraction, channel validation_logic,
/// engine dispatch with timeout, and the match on the processing outcome
/// (timeout / engine error / workflow errors / success). Every failure
/// branch routes through
/// [`report_failure_and_dlq`]. The outer `consume_loop` is responsible
/// for backpressure, shutdown, retries, and offset commit after this
/// returns.
async fn process_one_kafka_message(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
    // True only on the first attempt, so per-message outcome counters are
    // emitted once rather than once per retry (K10).
    count_outcome: bool,
) -> MsgOutcome {
    let topic = msg.topic().to_string();
    let channel = match ctx.topic_map.get(&topic) {
        Some(ch) => ch.clone(),
        None => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: "unknown",
                    topic: &topic,
                    payload: msg.payload().unwrap_or_default(),
                    message_status: "error",
                    error_kind: "kafka_unmapped_topic",
                    log_msg: "No channel mapping for Kafka topic",
                    dlq_reason: &format!("No channel mapping for topic '{topic}'"),
                },
                count_outcome,
            )
            .await;
        }
    };

    let payload = match msg.payload_view::<str>() {
        Some(Ok(text)) => text,
        Some(Err(e)) => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: msg.payload().unwrap_or_default(),
                    message_status: "error",
                    error_kind: "kafka_decode",
                    log_msg: "Failed to decode Kafka message payload as UTF-8",
                    dlq_reason: &format!("UTF-8 decode error: {e}"),
                },
                count_outcome,
            )
            .await;
        }
        None => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: &[],
                    message_status: "error",
                    error_kind: "kafka_empty_payload",
                    log_msg: "Empty Kafka message payload",
                    dlq_reason: "Empty message payload",
                },
                count_outcome,
            )
            .await;
        }
    };

    let data: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "kafka_parse",
                    log_msg: "Failed to parse Kafka message as JSON",
                    dlq_reason: &format!("JSON parse error: {e}"),
                },
                count_outcome,
            )
            .await;
        }
    };

    // Extract W3C trace context from Kafka message headers and attach it as
    // parent of the current tracing span (held for the rest of this scope).
    let _parent_cx = extract_kafka_trace_context(msg);

    // S1: apply the target channel's validation_logic before dispatch,
    // mirroring the HTTP ingress path. CORS / dedup / response cache are
    // HTTP-transport concerns and don't apply here. Failures are not
    // silently dropped — they record metrics, log, route to the DLQ when
    // one is configured, and commit the offset only on a confirmed DLQ
    // write (same outcome model as every other failure class).
    let metadata = kafka_metadata_value(&channel, &topic, msg);
    // F35: a quarantined channel is refused here too. Routed to the DLQ
    // rather than dropped, so the messages are replayable once the operator
    // fixes the channel's stored config.
    let channel_runtime = match ctx.channel_registry.require_serviceable(&channel).await {
        Ok(runtime) => runtime,
        Err(e) => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "channel_quarantined",
                    log_msg: "Kafka message for a channel that failed to load",
                    dlq_reason: &e.to_string(),
                },
                count_outcome,
            )
            .await;
        }
    };
    if let Err(e) = crate::channel::guards::validate_input(
        &channel,
        &channel_runtime,
        &data,
        &metadata,
        &ctx.datalogic,
    ) {
        return report_failure_and_dlq(
            ctx,
            FailureReport {
                channel: &channel,
                topic: &topic,
                payload: payload.as_bytes(),
                message_status: "error",
                error_kind: "kafka_validation",
                log_msg: "Kafka message rejected by channel validation_logic",
                dlq_reason: &format!("Validation failed: {e}"),
            },
            count_outcome,
        )
        .await;
    }

    let start = Instant::now();
    let mut message = dataflow_rs::Message::from_value(&data);
    crate::engine::utils::merge_metadata(&mut message, &metadata);

    // Clone the inner Arc<Engine> and release the lock immediately.
    let engine_ref = crate::engine::acquire_engine_read(&ctx.engine).await;
    let process_result = crate::engine::run_for_channel(
        &engine_ref,
        &channel,
        &mut message,
        Some(ctx.processing_timeout_ms),
        None,
        false,
    )
    .await;

    match process_result {
        Err(_) => {
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "timeout",
                    error_kind: "kafka_timeout",
                    log_msg: "Kafka message processing timed out",
                    dlq_reason: &format!(
                        "Processing timed out after {}ms",
                        ctx.processing_timeout_ms
                    ),
                },
                count_outcome,
            )
            .await
        }
        Ok((Err(e), _)) => {
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "kafka_processing",
                    log_msg: "Failed to process Kafka message",
                    dlq_reason: &format!("Processing error: {e}"),
                },
                count_outcome,
            )
            .await
        }
        Ok((Ok(()), _)) if message.has_errors() => {
            // v3 contract: workflow failures are pushed to
            // message.errors() while the outer Result stays Ok.
            let summary = message
                .errors()
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: &channel,
                    topic: &topic,
                    payload: payload.as_bytes(),
                    message_status: "error",
                    error_kind: "kafka_processing",
                    log_msg: "Kafka message processed with workflow errors",
                    dlq_reason: &format!("Workflow errors: {summary}"),
                },
                count_outcome,
            )
            .await
        }
        Ok((Ok(()), _)) => {
            let duration = start.elapsed().as_secs_f64();
            metrics::record_message(&channel, "ok");
            metrics::record_message_duration(&channel, duration);
            tracing::debug!(
                topic = %topic,
                channel = %channel,
                "Kafka message processed successfully"
            );
            MsgOutcome::Processed
        }
    }
}

/// Extract a W3C trace context from a Kafka message's headers and attach
/// it as the parent of the current tracing span. Returns the propagated
/// `opentelemetry::Context` so the caller can keep it in scope.
fn extract_kafka_trace_context(
    msg: &rdkafka::message::BorrowedMessage<'_>,
) -> opentelemetry::Context {
    use rdkafka::message::Headers;

    let mut header_map = HashMap::new();
    if let Some(headers) = msg.headers() {
        for idx in 0..headers.count() {
            if let Ok(header) = headers.get_as::<str>(idx)
                && let Some(value) = header.value
            {
                header_map.insert(header.key.to_string(), value.to_string());
            }
        }
    }
    crate::server::trace_context::set_parent_from_map(&header_map)
}

/// Initial delay between in-place retries of an uncommittable message.
/// Shared with the consumer restart supervisor (K7).
pub(crate) const INITIAL_RETRY_BACKOFF_MS: u64 = 1_000;
/// Cap for the exponential retry backoff.
const MAX_RETRY_BACKOFF_MS: u64 = 60_000;

/// Default in-place retry budget: 80% of librdkafka's default
/// `max.poll.interval.ms` (300s). The budget must stay safely below that
/// interval because the retry loop blocks polling, and a consumer that
/// stops polling for longer than `max.poll.interval.ms` is evicted from
/// the group while still working (and finally committing) the message.
pub(super) const DEFAULT_IN_PLACE_RETRY_BUDGET_MS: u64 = 240_000;

/// How long [`process_until_committed`] may keep one message in the
/// in-place retry loop before rewinding the partition and returning to the
/// poll loop: 80% of `max.poll.interval.ms` when `kafka.extra_config` sets
/// it, else [`DEFAULT_IN_PLACE_RETRY_BUDGET_MS`] (the same 80% of
/// librdkafka's 300s default). A value librdkafka would reject
/// (non-numeric) falls back to the default — consumer creation refuses the
/// property before the budget could ever be used.
pub(super) fn in_place_retry_budget_ms(extra_config: &HashMap<String, String>) -> u64 {
    extra_config
        .get("max.poll.interval.ms")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v / 5 * 4)
        .unwrap_or(DEFAULT_IN_PLACE_RETRY_BUDGET_MS)
}

/// Double the retry backoff, capped at [`MAX_RETRY_BACKOFF_MS`].
pub(crate) fn next_backoff_ms(current_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(MAX_RETRY_BACKOFF_MS)
}

/// Process one message until its offset can be committed, retrying it in
/// place with capped exponential backoff while the outcome is
/// [`MsgOutcome::Failed`] (see the module doc for the delivery guarantee).
/// Returns `false` when shutdown was requested mid-retry — the offset is
/// left uncommitted so the message is redelivered after restart.
///
/// K8 — what the revocation checks can and cannot see: rdkafka dispatches
/// rebalance callbacks only from the thread polling the consumer queue, and
/// this task awaits processing inline, so no poll runs while a message is
/// being worked. [`super::context::RebalanceState::is_revoked`] can
/// therefore flip only inside `recv()`, between messages — never mid-retry.
/// The checks below catch a revocation dispatched by an earlier poll (the
/// message was already handed to this loop); they cannot observe one that
/// arrives while it sleeps.
///
/// That blind spot is why the in-place retry is *bounded*
/// ([`in_place_retry_budget_ms`]): retrying forever would also block
/// polling past `max.poll.interval.ms`, get this consumer evicted from the
/// group, and keep it working — and finally committing — a partition it no
/// longer owns. When the budget expires the message is neither committed
/// nor dropped: the partition is rewound to the message's offset and
/// control returns to the poll loop, so rebalance callbacks fire and the
/// same message is redelivered (at-least-once preserved; head-of-line
/// blocking on a poison message persists, as the module doc documents).
pub(super) async fn process_until_committed(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_millis(ctx.retry_budget_ms);
    let mut backoff_ms = INITIAL_RETRY_BACKOFF_MS;
    let mut attempt: u64 = 0;
    loop {
        if abandon_if_revoked(ctx, msg, "before processing") {
            return true;
        }
        let outcome = process_one_kafka_message(ctx, msg, attempt == 0).await;
        if outcome.commits_offset() {
            // A revocation dispatched by the poll that delivered this
            // message means its offset is the new owner's to commit, not
            // ours (the flag cannot have flipped since — see above).
            if abandon_if_revoked(ctx, msg, "after processing") {
                return true;
            }
            commit_offset(ctx, msg);
            return true;
        }
        attempt += 1;
        metrics::record_error("kafka_retry");
        // Would the next sleep plus a worst-case attempt overrun the retry
        // budget? Stop retrying in place before that can happen, so the
        // total time away from the poll loop stays safely below
        // max.poll.interval.ms. (`processing_timeout_ms` bounds the engine
        // dispatch — the dominant cost of an attempt — so it is the
        // projection used for "worst case".)
        let projected_ms = backoff_ms.saturating_add(ctx.processing_timeout_ms);
        if Instant::now() + std::time::Duration::from_millis(projected_ms) >= deadline {
            seek_back_for_redelivery(ctx, msg, attempt);
            return true;
        }
        tracing::error!(
            topic = %msg.topic(),
            partition = msg.partition(),
            offset = msg.offset(),
            attempt,
            backoff_ms,
            "Kafka message failed without a confirmed DLQ write; offset not committed, retrying in place"
        );
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return false;
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)) => {}
        }
        backoff_ms = next_backoff_ms(backoff_ms);
    }
}

/// The in-place retry budget is exhausted (K8): rewind the partition to the
/// message's offset and go back to the poll loop. The next `recv()` both
/// lets rdkafka dispatch any pending rebalance callbacks and redelivers
/// this same message — neither committed nor dropped, so at-least-once
/// holds. A failed seek on a partition that was concurrently revoked is
/// harmless (the new owner redelivers from the last committed offset); on
/// a partition this consumer still owns it means the fetch position stays
/// past the message until the next rebalance or restart, so it is logged
/// as an error.
fn seek_back_for_redelivery(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
    attempts: u64,
) {
    metrics::record_error("kafka_retry_budget_exhausted");
    match ctx.consumer.seek(
        msg.topic(),
        msg.partition(),
        rdkafka::Offset::Offset(msg.offset()),
        std::time::Duration::from_secs(5),
    ) {
        Ok(()) => tracing::warn!(
            topic = %msg.topic(),
            partition = msg.partition(),
            offset = msg.offset(),
            attempts,
            budget_ms = ctx.retry_budget_ms,
            "In-place retry budget exhausted; partition rewound so the message is redelivered through the poll loop (offset not committed)"
        ),
        Err(e) => tracing::error!(
            topic = %msg.topic(),
            partition = msg.partition(),
            offset = msg.offset(),
            error = %e,
            "Failed to rewind partition after exhausting the retry budget; if this consumer still owns the partition, the message is not redelivered until the next rebalance or restart"
        ),
    }
}

/// `true` when the message's partition was revoked (K8): records the metric,
/// logs where in the lifecycle the revocation was noticed, and tells the
/// caller to abandon the message uncommitted for its new owner.
fn abandon_if_revoked(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
    stage: &'static str,
) -> bool {
    if !ctx.rebalance.is_revoked(msg.topic(), msg.partition()) {
        return false;
    }
    metrics::record_error("kafka_partition_revoked");
    tracing::warn!(
        topic = %msg.topic(),
        partition = msg.partition(),
        offset = msg.offset(),
        stage,
        "Partition revoked; abandoning message uncommitted for its new owner"
    );
    true
}

/// Commit the offset for a consumed message. The commit is asynchronous;
/// its result surfaces in the context's `commit_callback`, and the
/// next-to-consume offset is recorded so a revocation can flush it
/// synchronously in `pre_rebalance` (K8) — a rebalance does *not* restore
/// an in-flight async commit on its own. A commit lost anyway (enqueue
/// failure, revocation flush failure) risks redelivery, never message loss.
fn commit_offset(ctx: &ConsumeLoopContext, msg: &rdkafka::message::BorrowedMessage<'_>) {
    use rdkafka::consumer::CommitMode;
    match ctx.consumer.commit_message(msg, CommitMode::Async) {
        Ok(()) => ctx
            .rebalance
            .record_committable(msg.topic(), msg.partition(), msg.offset() + 1),
        Err(e) => tracing::error!(error = %e, "Failed to commit Kafka offset"),
    }
}

/// Build the Kafka-specific metadata object (channel, topic, key, partition,
/// offset) for an ingested message. Used both as validation_logic context and
/// as the metadata merged into the dispatched dataflow message, so validation
/// sees exactly what the workflow will see. The `channel` key (F4) labels
/// circuit-breaker state and connector metrics for this ingest path.
fn kafka_metadata_value(
    channel: &str,
    topic: &str,
    msg: &rdkafka::message::BorrowedMessage<'_>,
) -> serde_json::Value {
    use rdkafka::Message as KafkaMsg;

    let mut meta = serde_json::json!({
        "channel": channel,
        "kafka_topic": topic,
        "kafka_partition": msg.partition(),
        "kafka_offset": msg.offset(),
    });
    if let Some(key) = msg.key().and_then(|k| std::str::from_utf8(k).ok()) {
        meta["kafka_key"] = serde_json::json!(key);
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_commit_decision() {
        // Only a successful run or a confirmed DLQ write may advance the
        // offset — anything else must leave the message for redelivery.
        assert!(MsgOutcome::Processed.commits_offset());
        assert!(MsgOutcome::DeadLettered.commits_offset());
        assert!(!MsgOutcome::Failed.commits_offset());
    }

    /// K8: the in-place retry window must stay safely below
    /// `max.poll.interval.ms` (librdkafka default 300s) or the group evicts
    /// the consumer mid-retry.
    #[test]
    fn test_retry_budget_defaults_below_default_max_poll_interval() {
        assert_eq!(in_place_retry_budget_ms(&HashMap::new()), 240_000);
    }

    #[test]
    fn test_retry_budget_derives_from_configured_max_poll_interval() {
        let extra = HashMap::from([("max.poll.interval.ms".to_string(), "100000".to_string())]);
        assert_eq!(in_place_retry_budget_ms(&extra), 80_000);
    }

    /// librdkafka refuses a non-numeric value at client creation, so the
    /// fallback only has to be sane, never load-bearing.
    #[test]
    fn test_retry_budget_ignores_unparseable_values() {
        let extra = HashMap::from([("max.poll.interval.ms".to_string(), "ten".to_string())]);
        assert_eq!(
            in_place_retry_budget_ms(&extra),
            DEFAULT_IN_PLACE_RETRY_BUDGET_MS
        );
    }

    #[test]
    fn test_retry_backoff_doubles_and_caps() {
        let mut backoff = INITIAL_RETRY_BACKOFF_MS;
        assert_eq!(backoff, 1_000);
        backoff = next_backoff_ms(backoff);
        assert_eq!(backoff, 2_000);
        backoff = next_backoff_ms(backoff);
        assert_eq!(backoff, 4_000);
        while backoff < MAX_RETRY_BACKOFF_MS {
            backoff = next_backoff_ms(backoff);
        }
        assert_eq!(backoff, MAX_RETRY_BACKOFF_MS);
        // Capped: further retries never exceed the max, and no overflow
        assert_eq!(next_backoff_ms(MAX_RETRY_BACKOFF_MS), MAX_RETRY_BACKOFF_MS);
        assert_eq!(next_backoff_ms(u64::MAX), MAX_RETRY_BACKOFF_MS);
    }
}
