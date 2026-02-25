# Kafka Integration

[← Back to README](../README.md)

> Requires the `kafka` feature flag: `cargo build --features kafka`

## Topic-to-Channel Mapping

Map Kafka topics to Orion channels in your config:

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

## Metadata Injection

Kafka metadata is automatically injected into every message's metadata:

| Field | Description |
|-------|-------------|
| `kafka_topic` | Source topic name |
| `kafka_key` | Message key (if present) |
| `kafka_partition` | Partition number |
| `kafka_offset` | Offset within partition |

## Dead Letter Queue

Failed messages are routed to a configurable DLQ topic with structured error metadata:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"    # default
```

DLQ messages include the source topic, error details, original payload, and timestamp.

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
