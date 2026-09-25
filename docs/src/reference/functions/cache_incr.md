<!-- description: The cache_incr task function: atomically add to an integer on a Redis or in-memory cache connector, with a TTL applied only when the key is created. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache_incr`

Atomically adds to an integer on a cache connector and returns the new value.

## Synopsis

```json
{
  "name": "cache_incr",
  "input": {
    "connector": "redis",
    "key": "gen:ladder",
    "by": 1,
    "output": "temp_data.gen"
  }
}
```

## Description

`cache_incr` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

A missing key counts as `0` and is created. On Redis the increment is `INCRBY`, run in one script with the TTL so the two cannot be separated. Two callers bumping the same key at the same moment always get different values, which is what makes it a **generation counter**. A write bumps `gen:ladder`, and every cached read embeds the generation it read in its own key:

```json
[
  { "id": "gen", "name": "Read generation",
    "function": { "name": "cache_read", "input": { "connector": "redis", "key": "gen:ladder", "output": "temp_data.gen" } } },
  { "id": "hit", "name": "Read cached standings",
    "function": { "name": "cache_read", "input": { "connector": "redis",
      "key": { "cat": ["ladder:", { "var": "temp_data.gen" }] }, "output": "data.standings" } } }
]
```

After the bump, no read computes the old key again. The old entries are never served and expire at their own TTL. A route that depends on several counters reads them in one round trip with [`cache_read`](./cache_read.md)'s `keys`.

`ttl_secs` applies **only when this call creates the key**. Later bumps keep the expiry the key was created with. A counter bumped on every write still ends a fixed time after it was first made. A key that holds something other than an integer fails the task. A value `cache_write` stored as a JSON number is an integer.

A connector whose `write` [operation gate](../connectors/cache.md) is off refuses the call.

**Retry safety:** `unsafe_write`. A retry adds `by` a second time. For a generation counter that only costs one extra miss; for a counter people read, it is a wrong number. See [Retry safety](./retry-safety.md).

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string \| JSONLogic | yes | — | Counter key. A missing key counts as `0` and is created |
| `by` | integer \| JSONLogic | no | `1` | Amount to add; negative decrements |
| `ttl_secs` | number \| JSONLogic | no | no expiry | Time-to-live in seconds, applied only when this call creates the key |
| `output` | string \| JSONLogic | no | nothing recorded | Dotted path where the new value is written |

## Examples

```json
{ "name": "cache_incr", "input": { "connector": "redis", "key": "idle:worker-7", "ttl_secs": 300 } }
```

## Related

- [`cache_read`](./cache_read.md): reading one or several counters.
- [`cache_delete`](./cache_delete.md): deleting exact keys instead.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
