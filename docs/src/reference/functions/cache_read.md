<!-- description: The cache_read task function: read one key from a Redis or in-memory cache connector into the data context, with null for a key that is missing. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `cache_read`

Reads a key from a cache connector (Redis or the built-in in-memory backend).
Missing keys yield `null`.

## Synopsis

```json
{
  "name": "cache_read",
  "input": {
    "connector": "redis",
    "key": "rate:42",
    "output": "data.cached"
  }
}
```

## Description

`cache_read` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string | yes | — | Cache key to read |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the value is written |

## Examples

```json
{ "name": "cache_read", "input": { "connector": "redis", "key": "rate:42", "output": "data.cached" } }
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
