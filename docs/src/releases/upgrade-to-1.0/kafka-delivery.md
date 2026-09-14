<!-- description: Kafka delivery became at-least-once in Orion 1.0, and every ingress now applies the channel's rate limit, deduplication and backpressure. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Kafka delivery and ingress guards

Break 4 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**What changed.** The consumer used to commit the offset unconditionally,
whatever happened to the message, so a message that failed processing was lost. Offsets now advance only on **successful processing** or a
**confirmed dead-letter write**. Everything else leaves the offset uncommitted and retries the same message in place. Backoff starts at 1 s and doubles to a 60 s cap. That covers validation rejection, UTF-8 decode failure, JSON parse failure, unmapped topic, empty payload, timeout, engine error and workflow errors. Each retry cycle increments `orion_errors_total{reason="kafka_retry"}`.

**How you'll notice.** With `kafka.dlq.enabled = false`, which is still the
default — a permanently poisonous message **stalls the consumer indefinitely**. There is no give-up path: the message is never dropped and its offset is never committed. The in-place retrying *is* bounded, but only so that the consumer keeps polling. One cycle runs for at most **80% of `max.poll.interval.ms`**. That is 240 s against librdkafka's 300 s default, or 80% of whatever you set in `kafka.extra_config`. On expiry the consumer seeks the partition back to the message's offset and returns to the poll loop. The very same message is redelivered and the cycle starts over. The stall is therefore unchanged from an operator's point of view. The consumer keeps polling: it stays in its group instead of being evicted for exceeding `max.poll.interval.ms`, and rebalance callbacks stay live. Because messages are processed sequentially, this halts **every partition of every subscribed topic** on that instance, not only the poison message's partition. Restarting does not help — the offset was never committed, so the same message is redelivered.

Symptoms: consumer lag climbing on all partitions, and the same message logged repeatedly. Also `orion_errors_total{reason="kafka_retry"}` incrementing on a ~60 s cadence, and `orion_errors_total{reason="kafka_retry_budget_exhausted"}` incrementing once per budget expiry.

**What to do.** Enable the dead-letter queue. This is the recommended action
and turns the stall into an advancing offset plus a message you can inspect:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"   # default
```

```bash
ORION_KAFKA__DLQ__ENABLED=true
ORION_KAFKA__DLQ__TOPIC=orion-dlq
```

The DLQ envelope is `{"source_topic", "error", "original_payload", "timestamp"}`. Create the topic ahead of time if your broker has auto-creation disabled. A DLQ write that fails is *not* a confirmed write, and the message keeps retrying.

If you are already stalled without a DLQ, there are three options. Enable the DLQ and restart, fix the workflow or channel so the message processes, or advance the consumer-group offset externally with `kafka-consumer-groups --reset-offsets`. Removing the topic → channel mapping does **not** help; unmapped topics take the same failure path.

> **Do not set `enable.auto.commit` in `kafka.extra_config`.** The passthrough
> is applied last and would override the manual-commit setting this guarantee
> depends on.
>
> **`kafka.dlq.*` is unrelated to `trace_queue.dlq_*`.** The latter is the trace DLQ
> — a database table for failed trace persistence, with its own retry loop.

### Every ingress applies the channel's rate limit, dedup and backpressure

**What changed.** The Kafka ingress applied only `validation_logic`. It now runs the same guard set as HTTP, which adds `rate_limit`, `deduplication`, `backpressure` and `timeout_ms`. The `channel_call` and `/async` ingresses were similarly partial, and are now complete too. Audit any active channel that declares one of these blocks and is reached by an ingress other than synchronous HTTP. That means a Kafka topic, a `/async` submission, or a `channel_call` target:

```sql
SELECT name, config_json FROM current_channels
WHERE status = 'active'
  AND (config_json LIKE '%rate_limit%'
    OR config_json LIKE '%deduplication%'
    OR config_json LIKE '%backpressure%'
    OR config_json LIKE '%timeout_ms%');
```

**How you'll notice.**

- **Rate limit and backpressure throttle the topic.** A record refused because
  the channel is over its limit or at capacity is **not** dead-lettered: the
  offset is left uncommitted and the consumer retries in place with its
  existing capped backoff, then rewinds the partition when the retry budget
  expires. That backoff is the throttle. Expect consumer lag rather than
  errors, and watch `orion_errors_total{reason="kafka_guard_deferred"}` — a
  sustained rate means the topic is being throttled, not that records are being
  lost. Size `requests_per_second` / `max_concurrent_per_node` against the
  topic's real throughput before upgrading.
- **Deduplication suppresses records.** The idempotency key is the record
  header named by `deduplication.header`; if the record carries no such header,
  the **record key** is used. Record keys are usually partition keys, so if
  yours is an *entity* id (a customer, an account) rather than an *event* id,
  every record after the first inside `window_secs` is suppressed and counted
  as `orion_messages_total{status="duplicate"}`. Either set the header on the
  producer, or drop `deduplication` from channels fed by such a topic. A record
  identified as a duplicate is skipped and its offset committed — nothing is
  dead-lettered, because nothing failed. A redelivery of an offset that was
  never committed is recognized as the *same* delivery and runs, so
  at-least-once is intact.
- **`timeout_ms` is clamped, not adopted.** Kafka caps the channel value at
  `kafka.processing_timeout_ms` and `/async` at
  `trace_queue.processing_timeout_ms`. Those two settings are ceilings, not
  defaults: a Kafka dispatch blocks the consumer's poll loop and an `/async`
  dispatch occupies one of a fixed number of queue workers. A channel may
  shorten its deadline anywhere and lengthen it only where nothing shared
  depends on it. Previously these ingresses ignored `timeout_ms` entirely, so a
  channel with a short one and slow background work will now time out where it
  used to complete — raise the channel value if the HTTP deadline was only ever
  meant to bound the synchronous path, and raise the *transport* setting rather
  than the channel's if you need longer there.
- **`channel_call` spends the target channel's rate-limit budget** (bucket key:
  the calling channel, unless `key_logic` says otherwise), so a fan-out that
  calls one channel N times per request needs headroom for N. A refused call
  now surfaces as `429` or `503` instead of `500 ENGINE_ERROR`; clients
  matching on `ENGINE_ERROR` for these conditions need updating. Deduplication
  is deliberately **not** applied to `channel_call` — it would inherit the
  originating request's idempotency key and reject the second call of a
  legitimate fan-out.

**What to do.** Nothing is required. If a Kafka channel carries a `rate_limit`
intended as an HTTP-only control, either remove it or give it a `key_logic` that distinguishes the ingress. The default bucket key is the topic on Kafka and the client IP over HTTP. The same limit is a per-caller rate on each ingress rather than one shared cap.

### The platform limiter and the channel's now stack

**What changed.** The pre-1.0 middleware skipped the platform `[rate_limit]` data budget in one case. That was a request whose target channel declared its own `rate_limit` block, and whose limiter admitted it. A channel-level limit *replaced* the platform one. The two now layer. Every `/api/v1/data` request is metered against the platform budget (`endpoints.data_rps`, else `default_rps`) first, then against the channel's own limit in the ingress guards. A channel deliberately configured with a higher rate than the platform default used to be served at the channel's rate. It is now clamped to the platform's.

**How you'll notice.** With `rate_limit.enabled = true`, a high-volume channel starts answering `429` at the platform rate after the upgrade. That happens when its `rate_limit.requests_per_second` exceeds `endpoints.data_rps` (or `default_rps`). The counter `orion_rate_limit_rejections_total{group="data"}` climbs while the channel's own limit never trips.

**What to do.** Treat the platform budget as a backstop *above* every channel's limit, not a default that channels override. Raise `rate_limit.endpoints.data_rps` (or `default_rps`) to at least the largest per-channel `requests_per_second`. Both limiters key by client IP by default, so the comparison is per caller.

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
