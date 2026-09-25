<!-- description: The cache_read task function: read one key, or several in one round trip, from a Redis or in-memory cache connector into the data context, with null for a key that is missing. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache_read`

Reads a key, or several keys at once, from a cache connector (Redis or the
built-in in-memory backend). Missing keys yield `null`.

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

`keys` in place of `key` reads several keys in one round trip (one `MGET` on Redis) and writes an array in the same order, with `null` for each miss. A route that depends on two [generation counters](./cache_incr.md) reads both before it looks up its entry, in one call. Exactly one of `key` and `keys` is required; the workflow is refused at save time otherwise.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string \| JSONLogic | one of | — | Cache key to read |
| `keys` | array \| JSONLogic | one of | — | Several keys to read, at most 1000. Each element may be an expression, or the field may be one expression evaluating to an array. The result is an array in the same order |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the value is written |

## Examples

```json
{ "name": "cache_read", "input": { "connector": "redis", "key": "rate:42", "output": "data.cached" } }
```

```json
{ "name": "cache_read", "input": { "connector": "redis", "keys": ["gen:ladder", "gen:season"], "output": "temp_data.gens" } }
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
