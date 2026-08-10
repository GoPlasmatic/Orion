# Extensibility

Orion integrates with external systems through connectors, exposes custom logic via async function handlers, and supports multiple channel protocols for different ingestion patterns.

## Connectors

Connectors are named external service configurations. Secrets stay in
connectors, out of your workflows. The complete per-type reference — field
tables for `http`, `kafka`, `db`, `cache`, and `es`, auth schemes, header
precedence, `env://` and `vault://` secret references, operation gates,
retries, and secret masking — is in
[Connector Types](../reference/connectors.md).

## Kafka Consumer Configuration

<!-- TODO(docs2): moves to reference/configuration.md#kafka plus the Kafka
guide in Phases 3-4 (docs-implementation-plan.md T3.1/T4.2). -->

**Kafka consumer configuration:** map topics to channels in your config file:

```toml
[kafka]
enabled = true
brokers = ["localhost:9092"]
group_id = "orion"

[[kafka.topics]]
topic = "incoming-orders"
channel = "orders"
```

Async channels with `protocol: "kafka"` can also register topics via the API (DB-driven). Config-file and DB-driven topics are merged; duplicates are deduplicated with config-file entries taking precedence. The consumer restarts automatically on engine reload when the topic set changes.

**Metadata injection:** Kafka metadata is automatically injected into every message:

| Field | Description |
|-------|-------------|
| `kafka_topic` | Source topic name |
| `kafka_key` | Message key (if present) |
| `kafka_partition` | Partition number |
| `kafka_offset` | Offset within partition |

Access these in workflows via `{ "var": "metadata.kafka_topic" }`.

**Dead letter queue:** failed messages are routed to a configurable DLQ topic:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"
```

**Consumer settings:**

| Config | Default | Description |
|--------|---------|-------------|
| `kafka.processing_timeout_ms` | `60000` | Per-message processing timeout |
| `kafka.lag_poll_interval_secs` | `30` | Consumer lag polling interval |

Messages are processed strictly sequentially per consumer — required by the at-least-once commit contract. Scale throughput by running more instances in the same consumer group.

## Task Functions

Workflows compose built-in task functions: ten Orion connector handlers plus
the dataflow-rs parsing and transform library. Every function and its exact
`input` schema is cataloged in [Task Functions](../reference/functions.md).
The Orion handlers also surface machine-readable schemas at
`GET /api/v1/admin/functions`, and workflow create/update validates
`function.input` against them with field-pathed errors.

## Channel Protocols

A channel's protocol (`rest`, `http`, `kafka`), route pattern, and methods are
part of its channel configuration — specified in
[Channel Configuration › Routing & protocol](../reference/channel-config.md#routing--protocol).
