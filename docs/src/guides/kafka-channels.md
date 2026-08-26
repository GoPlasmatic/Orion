<!-- description: Run an Orion workflow for every record on a Kafka topic: how records arrive, what happens when one fails, and what processed-once does and does not mean. -->
# Consume from Kafka

A Kafka channel runs a workflow for every record on a topic. The workflow is the
same shape as an HTTP channel's — what changes is how records arrive, what
happens when one fails, and what "processed once" does and does not mean.

The repository ships a runnable example — `examples/packages/kafka-order-events`
— and the JSON below is included from it.

## 1. Enable the consumer

```toml
[kafka]
enabled = true
brokers = ["localhost:9092"]
group_id = "orion-prod"
```

Give each deployment its own `group_id`, or two environments will share offsets
and each will see half the records.

For a managed broker — Confluent Cloud, MSK, Aiven — add the auth block:

```toml
[kafka.auth]
security_protocol = "sasl_ssl"
sasl_mechanism = "PLAIN"
sasl_username = "env://KAFKA_API_KEY"
sasl_password = "env://KAFKA_API_SECRET"
```

These settings apply to every Kafka client Orion creates: the ingest consumer,
the `publish_kafka` producer, and the DLQ producer.

## 2. Map a topic to a channel

Two ways, and they merge:

```toml
# In the config file
[[kafka.topics]]
topic = "orders.events"
channel = "order-events"
```

```json
{{#include ../../../examples/packages/kafka-order-events/channel.json}}
```

Both sets are merged at startup, duplicates deduplicated with the config file
taking precedence. The consumer restarts automatically on engine reload when the
topic set changes, so adding a Kafka channel needs no restart.

**Prefer the channel-declared form.** It travels with the channel through
`package export`, which a config-file mapping does not.

A Kafka channel is created and activated like any other, and it does not need a
reachable broker to do so — the consumer registers on the next engine reload:

```bash
./examples/deploy.sh kafka-order-events
```

## 3. Read the record in the workflow

The record body is the payload, so the workflow starts with `parse_json` exactly
as an HTTP one does. The record's coordinates arrive as metadata:

| Field | Holds |
|---|---|
| `metadata.kafka_topic` | Source topic |
| `metadata.kafka_key` | Record key, when present |
| `metadata.kafka_partition` | Partition number |
| `metadata.kafka_offset` | Offset within the partition |

```json
{{#include ../../../examples/packages/kafka-order-events/workflow.json}}
```

Nothing in that workflow is Kafka-specific except the metadata paths — which is
the point. The same task pipeline would serve an HTTP channel; only the ingress
changed. Its logic is covered offline by
`examples/workflow-tests/kafka-order-events-*.case.json`, which run it through
the real engine with no broker.

## 4. Give poison messages somewhere to go

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"
```

> [!WARNING]
> **With the DLQ disabled, a failing message is retried in place with capped
> backoff — forever.** Nothing is lost, but one poison record can stall its
> partition until you notice. Enable the DLQ before the first production topic.

Delivery is at-least-once: an offset advances only on successful processing or a
**confirmed** DLQ write.

## What "processed once" actually means

This is the part worth reading twice.

- **Delivery is at-least-once.** A consumer restart, a rebalance, or a failure
  after processing but before the commit all redeliver the record.
- **Deduplication narrows that window; it does not close it.** A channel's
  `deduplication` block keyed on the record key or a header suppresses replays of
  a **settled** key. A redelivery of an attempt that never settled re-runs, by
  design — otherwise a crash mid-processing would lose the record.
- **So: `deduplication` does not make Kafka exactly-once.** If double execution
  would be harmful, make the downstream write idempotent as well — an upsert on
  a natural key rather than an insert.

The claim/settle mechanism is in
[Design Notes](../reference/design-notes.md#deduplication-claim-then-settle).

## Guards apply here too

A Kafka channel gets the same contract as an HTTP one, minus what the transport
cannot carry — there is no `Origin` header to check and no credential to
present, so `auth` and `origin_allow_list` do not apply. `rate_limit`,
`validation_logic`, `deduplication`, `backpressure`, and `timeout_ms` all do.

Throttling behaves differently from rejection, and the difference matters
operationally:

- **A record refused by the rate limit or backpressure is not dead-lettered.**
  Its offset stays uncommitted and the consumer's capped retry backoff becomes
  the throttle. You see this as **lag, not errors**.
- **The exception is a `key_logic` that cannot be evaluated** against the record.
  That fails identically on every redelivery, so the record is dead-lettered
  rather than blocking its partition.

`timeout_ms` is clamped to a transport ceiling, because a channel that exceeds
the consumer's poll interval would trigger a rebalance mid-processing. See
[Design Notes › Kafka's timeout clamp](../reference/design-notes.md#kafkas-timeout-clamp).

## Scale it

**Messages are processed strictly sequentially per consumer.** The at-least-once
commit contract requires it: committing an offset implicitly commits every
earlier offset on that partition, so a concurrent worker that finished early
would commit work that had not been done.

Throughput therefore scales by **running more instances in the same consumer
group**, up to the topic's partition count — not by raising a concurrency
setting. There is no such setting; the pre-1.0 `kafka.max_inflight` advertised
concurrency that never existed and was removed in 1.0.

In cluster mode each replica uses static group membership keyed by its
`instance_id`, so a rolling restart rejoins without a full group rebalance.

## Watch it

- **`/readyz` fails when Kafka ingestion is degraded**, so an unreachable broker
  pulls the node out of rotation rather than silently consuming nothing.
- **Lag climbing with no errors** is the throttling signature above — look for a
  sustained `kafka_guard_deferred` rate in `orion_errors_total`.
- **`orion-server test-connectivity`** probes the brokers before the server tries
  to start.

## Related

- [Configuration › Kafka](../reference/configuration.md#kafka) — every setting,
  including broker auth and the DLQ.
- [Channel Configuration](../reference/channel-config.md) — which guards apply
  on the Kafka ingress.
- [Troubleshooting](../operate/troubleshooting.md) — lag that is really
  throttling, and quarantined channels whose records go to the DLQ.
- [Channels](../concepts/channels.md) — where Kafka sits among the protocols.
