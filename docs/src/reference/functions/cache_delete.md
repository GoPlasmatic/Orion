<!-- description: The cache_delete task function: delete exact keys from a Redis or in-memory cache connector, so a write can drop the entries it made stale. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache_delete`

Deletes exact keys from a cache connector, and reports how many existed.

## Synopsis

```json
{
  "name": "cache_delete",
  "input": {
    "connector": "redis",
    "keys": [{ "cat": ["ladder:", { "var": "data.game" }] }],
    "output": "data.invalidated"
  }
}
```

## Description

`cache_delete` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

It is the invalidation half of caching a read in a workflow. The workflow that changes the data deletes the entries it made stale, rather than waiting out their TTL. On Redis the keys go in one `DEL`. A key that is not present is not an error; it is not counted.

Keys are exact. There is no prefix or pattern form, because on Redis one would be a `SCAN` over the whole keyspace. To retire a family of keys at once, embed a generation counter in each key and bump it with [`cache_incr`](./cache_incr.md). To invalidate a channel's response cache, use [`cache.namespaces`](../channel-config/cache.md#invalidation) and [`cache_invalidate`](./cache_invalidate.md).

A delete changes what later reads see, so a connector whose `write` [operation gate](../connectors/cache.md) is off refuses it, as it refuses `cache_write`. It reaches the same keyspace as `cache_read` and `cache_write`, never the channel deduplication store or response cache.

**Retry safety:** `idempotent_write`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `keys` | array \| JSONLogic | yes | — | Exact keys to delete, at most 1000. Each element may be an expression, or the field may be one expression evaluating to an array |
| `output` | string \| JSONLogic | no | nothing recorded | Dotted path where `{"deleted": n}` is written, `n` being the number of keys that existed |

## Examples

```json
{ "name": "cache_delete", "input": { "connector": "redis", "keys": ["ladder:chess", "ladder:go"] } }
```

## Related

- [`cache_incr`](./cache_incr.md): a generation counter, the alternative to deleting keys one by one.
- [`cache_read`](./cache_read.md) and [`cache_write`](./cache_write.md): the keys this deletes.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
