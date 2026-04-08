# Kafka Integration

[← Back to README](../README.md)

> Requires the `kafka` feature flag: `cargo build --features kafka`

## Topic-to-Channel Mapping

Map Kafka topics to Orion channels in your config file:

```toml
[kafka]
enabled = true
brokers = ["localhost:9092"]
group_id = "orion"

[[kafka.topics]]
topic = "incoming-orders"
channel = "orders"

[[kafka.topics]]
topic = "raw-events"
channel = "events"
```

### DB-Driven Topic Mapping

Async channels with `protocol: "kafka"` or `channel_type: "async"` that have a `topic` field are automatically registered as Kafka consumers at startup and on engine reload. This means you can add Kafka ingestion channels via the API without restarting Orion:

```json
{
  "name": "kafka-orders",
  "channel_type": "async",
  "protocol": "kafka",
  "topic": "incoming-orders",
  "consumer_group": "orion-orders",
  "workflow_id": "order-processing"
}
```

Config-file topics and DB-driven topics are merged — duplicates (by topic name) are deduplicated with config-file entries taking precedence. The Kafka consumer is automatically restarted on engine reload when the topic set changes.

## Metadata Injection

Kafka metadata is automatically injected into every message's metadata:

| Field | Description |
|-------|-------------|
| `kafka_topic` | Source topic name |
| `kafka_key` | Message key (if present) |
| `kafka_partition` | Partition number |
| `kafka_offset` | Offset within partition |

Access these in workflow conditions or transforms via `{ "var": "metadata.kafka_topic" }`.

## Dead Letter Queue

Failed messages are routed to a configurable DLQ topic with structured error metadata:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"    # default
```

DLQ messages include the source topic, error details, original payload, and timestamp.

Failed async traces are also stored in the `trace_dlq` database table with automatic retry support. Configure retry behavior:

```toml
[queue]
dlq_retry_enabled = true      # Enable DLQ retry processor
dlq_max_retries = 5           # Max retry attempts
dlq_poll_interval_secs = 30   # Retry poll interval
```

## Publishing to Kafka

Use the `publish_kafka` task function with optional JSONLogic for dynamic keys and values:

```json
{
  "function": {
    "name": "publish_kafka",
    "input": {
      "connector": "my_kafka",
      "topic": "processed-orders",
      "key_logic": { "var": "data.order_id" }
    }
  }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `connector` | Yes | Kafka connector name |
| `topic` | Yes | Target topic |
| `key_logic` | No | JSONLogic expression for partition key |
| `value_logic` | No | JSONLogic expression for message value (default: `message.data`) |

## Consumer Configuration

| Config | Default | Description |
|--------|---------|-------------|
| `kafka.brokers` | `["localhost:9092"]` | Broker addresses |
| `kafka.group_id` | `"orion"` | Consumer group ID |
| `kafka.processing_timeout_ms` | `60000` | Per-message processing timeout |
| `kafka.max_inflight` | `100` | Max in-flight messages |
| `kafka.lag_poll_interval_secs` | `30` | Consumer lag polling interval |
