<!-- description: The cache_invalidate task function: bump channel response-cache namespaces so every channel declaring them misses on its next request, on every store and node. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache_invalidate`

Invalidates channel response-cache namespaces, so every channel that declares one misses on its next request.

## Synopsis

```json
{
  "name": "cache_invalidate",
  "input": {
    "namespaces": ["ladder", { "cat": ["season:", { "var": "data.season" }] }]
  }
}
```

## Description

`cache_invalidate` is the write-side half of a channel's [`cache.namespaces`](../channel-config/cache.md#invalidation). The workflow that changes the data a namespace covers invalidates it in the same run. Every entry cached under the old version is no longer served. The call bumps one counter per namespace per store, with no scan and no delete.

It takes **no connector**. A response-cache entry can live in the default store or in any cache connector a channel names. The author should not have to know which. The function bumps each namespace in three places. The first is the default store, which is the shared Redis in cluster mode, so every node sees it. The others are every in-memory response-cache store on this node and every Redis cache connector whose `write` gate is on. A store it cannot reach is logged and skipped. The task then fails, after every reachable store has been bumped, and the channel's `ttl_secs` bounds what the unreached store serves.

It moves counters and nothing else. A workflow cannot write a response body into a channel's cache, and neither `cache_write` nor `cache_delete` reaches the response cache's keys.

The operator's form is [`POST /api/v1/admin/cache/namespaces/{namespace}/invalidate`](../admin-api/cache.md).

**Retry safety:** `idempotent_write`. A second bump retires nothing the first did not; the only cost is one more miss per channel. See [Retry safety](./retry-safety.md).

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `namespaces` | array \| JSONLogic | yes | — | Namespaces to invalidate, 1 to 64. Each element may be an expression, or the field may be one expression evaluating to an array. Each name is 1–64 of `a-z 0-9 _ - . :` |
| `output` | string \| JSONLogic | no | nothing recorded | Dotted path where `{"namespaces": n, "stores": m}` is written |

## Examples

```json
{ "name": "cache_invalidate", "input": { "namespaces": ["ladder"], "output": "temp_data.invalidated" } }
```

## Related

- [`cache`](../channel-config/cache.md#invalidation): declaring namespaces on a channel.
- [Cache endpoints](../admin-api/cache.md): the same invalidation from the admin API.
- [`cache_delete`](./cache_delete.md) and [`cache_incr`](./cache_incr.md): invalidating a workflow's own cached keys.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
