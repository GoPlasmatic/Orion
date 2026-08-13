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
    /// The channel's deduplication window holds this message's idempotency
    /// key under a *settled* claim, so an earlier delivery ran it to a
    /// committed outcome (N16). Nothing to run and nothing to preserve — the
    /// redelivery is the at-least-once transport doing its job, and skipping
    /// it is the point of configuring dedup on a Kafka channel.
    ///
    /// This never fires for a record's own unfinished attempts: the claim
    /// carries the record's coordinates as its owner, so a retry or a
    /// redelivery of an offset that was never committed recognises its own
    /// claim and processes the message (see `check_deduplication` in
    /// [`crate::channel::guards`]).
    Deduplicated,
    /// Processing failed but the payload was confirmed written to the DLQ.
    DeadLettered,
    /// Processing failed and the payload is not preserved anywhere (DLQ
    /// disabled, the DLQ write itself failed, or a guard deferred the
    /// message). Left uncommitted for redelivery.
    Failed,
}

impl MsgOutcome {
    /// Whether the message's offset may be committed. Committing an offset
    /// implicitly commits every earlier offset on the partition, so this
    /// must be true only when the message no longer needs redelivery.
    fn commits_offset(self) -> bool {
        matches!(
            self,
            MsgOutcome::Processed | MsgOutcome::Deduplicated | MsgOutcome::DeadLettered
        )
    }
}

/// What a guard refusal means for a Kafka message.
///
/// N16 gives the Kafka ingress the same guards as HTTP, but a Kafka record
/// cannot be answered with a status code — it can only be committed, dead
/// lettered, or left for redelivery. The three dispositions are exactly that
/// choice, and which one a refusal gets depends on whether the message is
/// wrong (terminal), already handled (duplicate), or merely unwelcome right
/// now (deferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GuardDisposition {
    /// The message will never be accepted: `validation_logic` rejected it.
    /// Dead letter it and commit — redelivering it forever is head-of-line
    /// blocking on a poison message.
    Terminal,
    /// A previous delivery of this key was already processed. Commit; do not
    /// dead letter, because nothing failed.
    Duplicate,
    /// The channel is over its rate limit or at capacity, or a guard backend
    /// is down and the channel fails closed. The message is fine and the
    /// condition is transient, so leave the offset uncommitted: the retry
    /// loop's capped backoff *is* the throttle, and the partition is rewound
    /// for redelivery when the budget runs out. Dead lettering a message for
    /// arriving during a busy second would be data loss dressed as a policy.
    Deferred,
}

/// Classify a guard refusal for the Kafka ingress.
///
/// `RateLimitKeyUnavailable` is deliberately *not* grouped with
/// `RateLimited`: being over a limit clears with time, but a
/// `rate_limit.key_logic` that cannot be evaluated against this record will
/// fail against every redelivery of it, so deferring it would block the
/// partition for as long as the channel keeps that expression. It is as
/// terminal as a `validation_logic` rejection and is dead-lettered the same
/// way.
pub(super) fn classify_guard_refusal(error: &crate::errors::OrionError) -> GuardDisposition {
    use crate::errors::OrionError;
    match error {
        OrionError::Conflict(_) => GuardDisposition::Duplicate,
        OrionError::RateLimited(_) | OrionError::ServiceUnavailable(_) => {
            GuardDisposition::Deferred
        }
        _ => GuardDisposition::Terminal,
    }
}

/// Decode + parse + dispatch a single Kafka message. Wraps the entire
/// per-message lifecycle: topic → channel lookup, payload UTF-8 decode,
/// JSON parse, W3C trace context extraction, the channel's ingress guards,
/// engine dispatch with the channel's deadline, and the match on the
/// processing outcome (timeout / engine error / workflow errors / success).
/// Every failure branch routes through [`report_failure_and_dlq`] or, for a
/// guard refusal, through [`classify_guard_refusal`]. The outer
/// `consume_loop` is responsible for shutdown, retries, and the offset
/// commit after this returns.
async fn process_one_kafka_message(
    ctx: &ConsumeLoopContext,
    msg: &rdkafka::message::BorrowedMessage<'_>,
    // True only on the first attempt, so per-message outcome counters are
    // emitted once rather than once per retry (K10).
    count_outcome: bool,
) -> MsgOutcome {
    let topic: &str = msg.topic();
    let channel: &str = match ctx.topic_map.get(topic) {
        Some(ch) => ch.as_str(),
        None => {
            return report_failure_and_dlq(
                ctx,
                FailureReport {
                    channel: "unknown",
                    topic,
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
                    channel,
                    topic,
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
                    channel,
                    topic,
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

    // Every failure branch from here on reports the same message identity
    // (channel / topic / payload) and the same counter policy, so a site only
    // spells out the four labels that distinguish it.
    let fail = async |message_status: &'static str,
                      error_kind: &'static str,
                      log_msg: &'static str,
                      dlq_reason: String| {
        report_failure_and_dlq(
            ctx,
            FailureReport {
                channel,
                topic,
                payload: payload.as_bytes(),
                message_status,
                error_kind,
                log_msg,
                dlq_reason: &dlq_reason,
            },
            count_outcome,
        )
        .await
    };

    let data: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return fail(
                "error",
                "kafka_parse",
                "Failed to parse Kafka message as JSON",
                format!("JSON parse error: {e}"),
            )
            .await;
        }
    };

    // Record headers, read once and shared by trace-context propagation and
    // the ingress guards below.
    let headers = kafka_headers_map(msg);

    // Extract W3C trace context from Kafka message headers and attach it as
    // parent of the current tracing span (held for the rest of this scope).
    let _parent_cx = crate::server::trace_context::set_parent_from_map(&headers);

    // S1/N16: apply the target channel's ingress guards before dispatch,
    // through the same function every other transport uses — which guards
    // run is `Transport::Kafka`'s row of the matrix. Failures are not
    // silently dropped: they record metrics, log, and are committed, dead
    // lettered or left for redelivery per `classify_guard_refusal`.
    let metadata = kafka_metadata_value(channel, topic, msg);
    // F35: a quarantined channel is refused here too. Routed to the DLQ
    // rather than dropped, so the messages are replayable once the operator
    // fixes the channel's stored config.
    let channel_runtime = match ctx.channel_registry.require_serviceable(channel) {
        Ok(runtime) => runtime,
        Err(e) => {
            return fail(
                "error",
                "channel_quarantined",
                "Kafka message for a channel that failed to load",
                e.to_string(),
            )
            .await;
        }
    };
    let header_lookup = |name: &str| headers.get(&name.to_ascii_lowercase()).cloned();
    // The record key is the natural idempotency key on an at-least-once
    // transport, used when the channel's dedup header is not among the
    // record headers.
    let record_key = msg.key().and_then(|k| std::str::from_utf8(k).ok());
    // The record's coordinates identify this *physical* delivery: every
    // redelivery of an offset that was never committed — an in-place retry, a
    // rewind after the retry budget, a restart — presents the same token, and
    // the dedup guard recognises its own unsettled claim instead of refusing
    // the message as a duplicate of itself.
    let dedup_owner = format!("kafka:{topic}/{}/{}", msg.partition(), msg.offset());
    let guard_result = crate::channel::guards::admit(crate::channel::guards::GuardRequest {
        transport: crate::channel::guards::Transport::Kafka,
        channel,
        runtime: &channel_runtime,
        data: &data,
        metadata: &metadata,
        datalogic: &ctx.datalogic,
        origin: None,
        // A Kafka channel consumes one topic, so the topic is a stable,
        // bounded identity: a channel's limit is a throughput cap on its
        // topic unless `key_logic` says otherwise.
        caller_identity: topic,
        header: &header_lookup,
        // Kafka does not authenticate per record — the broker connection
        // does (see `Transport::guards`), so there is no body to sign.
        raw_body: None,
        dedup_key_fallback: record_key,
        dedup_owner: Some(&dedup_owner),
        default_timeout_ms: Some(ctx.processing_timeout_ms),
        // K8: the dispatch blocks the poll loop, so the channel may shorten
        // this deadline but never lengthen it — see `process_until_committed`
        // for what the poll gap has to stay under.
        max_timeout_ms: Some(ctx.processing_timeout_ms),
    })
    .await;

    let admission = match guard_result {
        Ok(admission) => admission,
        Err(e) => match classify_guard_refusal(&e) {
            GuardDisposition::Duplicate => {
                if count_outcome {
                    metrics::record_message(channel, "duplicate");
                }
                tracing::debug!(
                    topic = %topic,
                    channel = %channel,
                    "Kafka message suppressed by the channel's deduplication window"
                );
                return MsgOutcome::Deduplicated;
            }
            GuardDisposition::Deferred => {
                metrics::record_error("kafka_guard_deferred");
                tracing::warn!(
                    topic = %topic,
                    channel = %channel,
                    error = %e,
                    "Kafka message deferred by a channel guard; offset not committed, will retry"
                );
                return MsgOutcome::Failed;
            }
            GuardDisposition::Terminal => {
                return fail(
                    "error",
                    "kafka_validation",
                    "Kafka message rejected by a channel ingress guard",
                    format!("Validation failed: {e}"),
                )
                .await;
            }
        },
    };
    // Held for the whole dispatch, so `max_concurrent_per_node` bounds Kafka
    // work alongside HTTP and `channel_call` work rather than around it.
    let _backpressure_permit = admission.backpressure_permit;
    // Settled below, once the outcome is known: an offset that will be
    // committed confirms the claim, and one that will not releases it, so a
    // redelivery is processed rather than mistaken for a completed delivery.
    let dedup_claim = admission.dedup_claim;
    // Clamped by the guard chain to `kafka.processing_timeout_ms` (K8), so a
    // channel's own `timeout_ms` can only shorten the poll gap.
    let processing_timeout_ms = admission.timeout_ms.unwrap_or(ctx.processing_timeout_ms);

    let start = Instant::now();
    // Kafka ingress carries no rollout bucket: a record has no sticky caller
    // identity and no forwarded IP, and a random bucket per record would split
    // one topic's traffic across canary versions non-deterministically. A
    // message with no bucket is admitted by every workflow, rollout or not.
    let mut message = dataflow_rs::Message::builder()
        .payload_json(&data)
        .metadata_json(&metadata)
        .build();

    // Clone the inner Arc<Engine> and release the lock immediately.
    let engine_ref = ctx.engine.load();
    let process_result = crate::engine::run_for_channel(
        &engine_ref,
        channel,
        &mut message,
        Some(processing_timeout_ms),
        None,
        None,
    )
    .await;

    let outcome = match process_result {
        Err(_) => {
            fail(
                "timeout",
                "kafka_timeout",
                "Kafka message processing timed out",
                format!("Processing timed out after {processing_timeout_ms}ms"),
            )
            .await
        }
        Ok((Err(e), _)) => {
            fail(
                "error",
                "kafka_processing",
                "Failed to process Kafka message",
                format!("Processing error: {e}"),
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
            fail(
                "error",
                "kafka_processing",
                "Kafka message processed with workflow errors",
                format!("Workflow errors: {summary}"),
            )
            .await
        }
        Ok((Ok(()), _)) => {
            let duration = start.elapsed().as_secs_f64();
            metrics::record_message(channel, "ok");
            metrics::record_message_duration(channel, duration);
            tracing::debug!(
                topic = %topic,
                channel = %channel,
                "Kafka message processed successfully"
            );
            MsgOutcome::Processed
        }
    };
    settle_dedup_claim(dedup_claim, outcome).await;
    outcome
}

/// Settle the idempotency key this delivery claimed at admission (N16).
///
/// The claim is taken *before* the workflow runs — that is the only point at
/// which the check is atomic against a concurrent delivery — so every path
/// out of the dispatch has to say what became of it:
///
/// * the offset will be committed (processed, or preserved in the DLQ), so
///   the key is **confirmed**: any further record carrying it, including a
///   replay of this one whose commit was lost, is a duplicate of work that
///   was done;
/// * the offset will not be committed, so the record is coming back and the
///   key is **released**. Leaving it claimed is how a message gets committed
///   without ever running: the retry reads the key its own previous attempt
///   wrote, `apply_guards` answers `409`, and the ingress reads that as
///   "already handled".
async fn settle_dedup_claim(
    claim: Option<crate::channel::guards::DedupClaim>,
    outcome: MsgOutcome,
) {
    let Some(claim) = claim else {
        return;
    };
    if outcome.commits_offset() {
        claim.confirm().await;
    } else {
        claim.release().await;
    }
}

/// A Kafka record's headers as a lookup map.
///
/// Keys are lowercased so a channel's configured header name resolves the
/// same way it does over HTTP: HTTP header names are case-insensitive and
/// Kafka's are not, so a producer spelling `Idempotency-Key` must still
/// satisfy a channel configured with `idempotency-key`. Read once per
/// message and shared by W3C trace-context propagation (whose keys are
/// lowercase by specification) and by the ingress guards.
fn kafka_headers_map(msg: &rdkafka::message::BorrowedMessage<'_>) -> HashMap<String, String> {
    use rdkafka::message::Headers;

    let mut header_map = HashMap::new();
    if let Some(headers) = msg.headers() {
        for idx in 0..headers.count() {
            if let Ok(header) = headers.get_as::<str>(idx)
                && let Some(value) = header.value
            {
                header_map.insert(header.key.to_ascii_lowercase(), value.to_string());
            }
        }
    }
    header_map
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
        // max.poll.interval.ms. (`kafka.processing_timeout_ms` bounds the
        // engine dispatch — the dominant cost of an attempt — so it is the
        // projection used for "worst case". A channel's own `timeout_ms` is
        // honoured on this path but *clamped* to that value by the guard
        // chain, precisely so this projection stays an upper bound: an
        // unclamped channel value would let one dispatch outlast the whole
        // budget and get the consumer evicted mid-message.)
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
        if !super::sleep_or_shutdown(shutdown_rx, backoff_ms).await {
            return false;
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
        // A successful run, a suppressed duplicate, or a confirmed DLQ write
        // may advance the offset — anything else must leave the message for
        // redelivery.
        assert!(MsgOutcome::Processed.commits_offset());
        assert!(MsgOutcome::Deduplicated.commits_offset());
        assert!(MsgOutcome::DeadLettered.commits_offset());
        assert!(!MsgOutcome::Failed.commits_offset());
    }

    /// N16: the Kafka ingress applies the same guards as HTTP, but it cannot
    /// answer with a status code — a refusal has to become a commit
    /// decision. This is that translation, and getting it wrong is either
    /// data loss (dead lettering a rate-limited message) or an infinite
    /// redelivery loop (retrying a message validation will never accept).
    #[test]
    fn guard_refusals_map_to_the_right_commit_decision() {
        use crate::errors::OrionError;

        // Validation rejected the payload: it will never be accepted, so
        // dead letter it rather than block the partition forever.
        assert_eq!(
            classify_guard_refusal(&OrionError::validation("Input validation failed")),
            GuardDisposition::Terminal
        );

        // The dedup window already holds this key: an earlier delivery did
        // the work. Commit, and do not dead letter — nothing failed.
        assert_eq!(
            classify_guard_refusal(&OrionError::Conflict("Duplicate request".into())),
            GuardDisposition::Duplicate
        );

        // Over the channel's rate limit, or at its concurrency cap, or a
        // guard backend is down on a fail-closed channel. The message is
        // fine; only the moment is wrong. Leave it uncommitted so the retry
        // backoff throttles the consumer instead of discarding traffic.
        assert_eq!(
            classify_guard_refusal(&OrionError::RateLimited("Too many requests".into())),
            GuardDisposition::Deferred
        );
        assert_eq!(
            classify_guard_refusal(&OrionError::ServiceUnavailable("at capacity".into())),
            GuardDisposition::Deferred
        );

        // A `rate_limit.key_logic` that cannot be evaluated is not "over a
        // limit": the expression fails on this record and on every copy of
        // it, so deferring would head-of-line block the partition for as long
        // as the channel keeps the expression. As terminal as a
        // `validation_logic` rejection, and dead-lettered the same way.
        assert_eq!(
            classify_guard_refusal(&OrionError::RateLimitKeyUnavailable(
                "Too many requests".into()
            )),
            GuardDisposition::Terminal
        );
    }

    /// The dedup claim is settled by the *commit* decision, not by success:
    /// anything that advances the offset confirms the key, anything that
    /// leaves the record for redelivery hands it back. Getting this backwards
    /// is exactly the message-loss bug the claim exists to prevent — a
    /// retried record reading the key its own previous attempt wrote,
    /// answered `409`, committed, and dropped.
    #[test]
    fn the_dedup_claim_is_settled_by_the_commit_decision() {
        for outcome in [MsgOutcome::Processed, MsgOutcome::DeadLettered] {
            assert!(
                outcome.commits_offset(),
                "{outcome:?} advances the offset, so its key is confirmed"
            );
        }
        assert!(
            !MsgOutcome::Failed.commits_offset(),
            "a failed delivery is coming back, so its key must be released"
        );
        // A duplicate never claimed anything — `apply_guards` refused before
        // returning an admission — so there is nothing to settle.
        assert!(MsgOutcome::Deduplicated.commits_offset());
    }

    /// A deferred message is retried, not committed and not dead lettered —
    /// which is what makes the retry loop's capped backoff the throttle.
    #[test]
    fn a_deferred_guard_refusal_leaves_the_offset_uncommitted() {
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
