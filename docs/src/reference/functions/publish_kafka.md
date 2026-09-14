<!-- description: The publish_kafka task function: publish a message to a Kafka topic through a Kafka connector, with a JSONLogic topic, key and value. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `publish_kafka`

Publishes a message to a Kafka topic through a Kafka connector. Requires Kafka to
be enabled in config. If `value` is omitted, the full data context is published.

## Synopsis

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": "order.placed",
    "key": {
      "var": "data.order.id"
    },
    "value": {
      "var": "data.order"
    }
  }
}
```

## Description

`publish_kafka` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `unsafe_write`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string \| JSONLogic | yes | — | Name of the Kafka connector. A computed name is not yet supported |
| `topic` | string \| JSONLogic | yes | — | Target topic. Accepts an expression, so one task can route by message content |
| `key` | any \| JSONLogic | no | — | The message key. Accepts the pre-1.0 name `key_logic` |
| `value` | any \| JSONLogic | no | full `data` | The message value. Accepts the pre-1.0 name `value_logic` |

## Examples

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": "order.placed",
    "key": { "var": "data.order.id" },
    "value": { "var": "data.order" }
  }
}
```

A computed `topic` is what lets one task fan a stream out by content: the tenant, the region, the event type. Before, that took one task per destination:

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": { "cat": ["orders.", { "var": "data.region" }] },
    "value": { "var": "data.order" }
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Kafka channels](../../guides/patterns/kafka-channels.md): consuming from Kafka, the other direction.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
