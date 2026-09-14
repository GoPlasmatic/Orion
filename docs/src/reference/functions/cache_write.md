<!-- description: The cache_write task function: write one key to a Redis or in-memory cache connector, JSON-serializing non-string values, with an optional TTL. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `cache_write`

Writes a key to a cache connector, optionally with a TTL.

## Synopsis

```json
{
  "name": "cache_write",
  "input": {
    "connector": "redis",
    "key": "rate:42",
    "value": 1,
    "ttl_secs": 60
  }
}
```

## Description

`cache_write` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `idempotent_write`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string | yes | — | Cache key to set |
| `value` | any | yes | — | Value to store (non-strings are JSON-serialized) |
| `ttl_secs` | number | no | no expiry | Time-to-live in seconds |

## Examples

```json
{ "name": "cache_write", "input": { "connector": "redis", "key": "rate:42", "value": 1, "ttl_secs": 60 } }
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
