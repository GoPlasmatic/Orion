<!-- description: Run an Orion workflow for every record on a Kafka topic: how records arrive, what happens when one fails, and what processed-once does and does not mean. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Consume from Kafka

A Kafka channel runs a workflow for every record on a topic, and the workflow is the same shape as an HTTP channel's. What changes is how records arrive, what happens when one fails, and what "processed once" means. The repository ships a runnable example, `examples/packages/kafka-order-events`, and the JSON below is included from it.

## Before you start

Tested with Orion 1.8.1. You need an Orion server, access to a Kafka broker, and permission to create or use a topic and a consumer group. The runnable example also needs Git, Docker with Compose, `curl`, Python 3 and a POSIX shell. Do not reuse a production consumer group while following the guide.

## 1. Enable the consumer

Turn Kafka on and name the brokers and the group:

```toml
[kafka]
enabled = true
brokers = ["localhost:9092"]
group_id = "orion-prod"
```

Give each deployment its own `group_id`, or two environments share offsets and each sees half the records. For a managed broker such as Confluent Cloud, MSK or Aiven, add the auth block:

```toml
[kafka.auth]
security_protocol = "sasl_ssl"
sasl_mechanism = "PLAIN"
sasl_username = "env://KAFKA_API_KEY"
sasl_password = "env://KAFKA_API_SECRET"
```

These settings apply to every Kafka client Orion creates: the ingest consumer, the `publish_kafka` producer, and the DLQ producer.

## 2. Map a topic to a channel

There are two ways, and they merge. In the config file:

```toml
[[kafka.topics]]
topic = "orders.events"
channel = "order-events"
```

Or on the channel itself:

```json
{{#include ../../../../examples/packages/kafka-order-events/channel.json}}
```

Both sets are merged at startup, with duplicates deduplicated and the config file taking precedence. The consumer restarts on engine reload when the topic set changes, so adding a Kafka channel needs no restart. Prefer the channel-declared form: it travels with the channel through `package export`, which a config-file mapping does not.

A Kafka channel is created and activated like any other, and it does not need a reachable broker to do so. The consumer registers on the next engine reload:

```bash
./examples/deploy.sh kafka-order-events
```

## 3. Read the record in the workflow

The record body is the payload, so the workflow starts with `parse_json` exactly as an HTTP one does. The record's coordinates arrive as metadata:

| Field | Holds |
|---|---|
| `metadata.kafka_topic` | Source topic |
| `metadata.kafka_key` | Record key, when present |
| `metadata.kafka_partition` | Partition number |
| `metadata.kafka_offset` | Offset within the partition |

```json
{{#include ../../../../examples/packages/kafka-order-events/workflow.json}}
```

Nothing in that workflow is Kafka-specific except the metadata paths, which is the point. The same task pipeline would serve an HTTP channel; only the ingress changed. Its logic is covered offline by `examples/workflow-tests/kafka-order-events-*.case.json`, which run it through the real engine with no broker.

## 4. Give poison messages somewhere to go

Enable the dead-letter topic before the first production topic:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"
```

> [!WARNING]
> With the DLQ disabled, a failing message is retried in place with capped backoff, forever. Nothing is lost, but one poison record can stall its partition until you notice.

Delivery is at-least-once: an offset advances only on successful processing or a confirmed DLQ write.

## What "processed once" means

This is the part worth reading twice.

- **Delivery is at-least-once.** A consumer restart, a rebalance, or a failure after processing but before the commit all redeliver the record.
- **Deduplication narrows that window; it does not close it.** A channel's `deduplication` block keyed on the record key or a header suppresses replays of a settled key. A redelivery of an attempt that never settled re-runs, by design. Otherwise a crash mid-processing would lose the record.
- **So `deduplication` does not make Kafka exactly once.** If double execution would be harmful, make the downstream write idempotent as well: an upsert on a natural key rather than an insert.

The claim-and-settle mechanism is in [Design notes](../../concepts/design-notes.md#deduplication-claim-then-settle).

## Guards apply here too

A Kafka channel gets the same contract as an HTTP one, minus what the transport cannot carry. There is no `Origin` header to check and no credential to present, so `auth` and `origin_allow_list` do not apply. `rate_limit`, `validation_logic`, `deduplication`, `backpressure` and `timeout_ms` all do.

Throttling behaves differently from rejection, and the difference matters operationally:

- **A record refused by the rate limit or backpressure is not dead-lettered.** Its offset stays uncommitted and the consumer's capped retry backoff becomes the throttle. You see this as lag, not errors.
- **The exception is a `key_logic` that cannot be evaluated** against the record. That fails identically on every redelivery, so the record is dead-lettered rather than blocking its partition.

`timeout_ms` is clamped to a transport ceiling, because a channel that exceeds the consumer's poll interval would trigger a rebalance mid-processing. See [Kafka's timeout clamp](../../concepts/design-notes.md#kafkas-timeout-clamp).

## Scale it

Messages are processed strictly sequentially per consumer. The at-least-once commit contract requires it. Committing an offset implicitly commits every earlier offset on that partition. A concurrent worker that finished early would commit work that had not been done.

Throughput therefore scales by running more instances in the same consumer group, up to the topic's partition count, not by raising a concurrency setting. There is no such setting; the pre-1.0 `kafka.max_inflight` advertised concurrency that never existed and was removed in 1.0. In cluster mode each replica uses static group membership keyed by its `instance_id`, so a rolling restart rejoins without a full group rebalance.

## Verify

Three signals tell you the consumer is doing what you think:

- `/readyz` fails when Kafka ingestion is degraded, so an unreachable broker pulls the node out of rotation rather than silently consuming nothing.
- Lag climbing with no errors is the throttling signature above. Look for a sustained `kafka_guard_deferred` rate in `orion_errors_total`.
- `orion-server test-connectivity` probes the brokers before the server tries to start.

## Next steps

- [Configuration › Kafka](../../reference/configuration/kafka.md): every setting, including broker auth and the DLQ.
- [Channel configuration](../../reference/channel-config/index.md): which guards apply on the Kafka ingress.
- [Troubleshooting](../../operate/maintain/troubleshooting.md): lag that is really throttling, and quarantined channels whose records go to the DLQ.
- [Channels](../../concepts/channels.md): where Kafka sits among the protocols.
