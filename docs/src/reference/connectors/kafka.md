<!-- description: The `kafka` connector config: the producer-only bootstrap servers, client id, compression, acks and the publish operation gate. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `kafka` connectors

The `config` fields of a `kafka` connector, which backs Kafka topics, produce only.

Produces to Kafka topics through [`publish_kafka`](../functions/publish_kafka.md). The connector is producer-only: consuming is configured under `[kafka]` in the [server config](../configuration/kafka.md), not here.

```json
{
  "name": "event-bus",
  "connector_type": "kafka",
  "config": {
    "type": "kafka",
    "brokers": ["kafka1:9092", "kafka2:9092"],
    "topic": "events"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `brokers` | array of strings | yes | — | Bare `host:port` entries, no URL scheme. An empty list `[]` publishes through the globally configured `[kafka]` cluster |
| `topic` | string | yes | — | The topic this connector is associated with. Each `publish_kafka` task names its own target topic |
| `allow_private_urls` | boolean | no | `false` | Allow brokers on private and internal IP addresses; entries are checked as host/port pairs (SSRF protection) |
| `operations` | object | no | all allowed | `publish` gate — see [Operation gates](./operation-gates.md) |

> [!NOTE]
> `publish_kafka` requires `kafka.enabled = true` in the server config, even when the connector names its own brokers.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
