<!-- description: The cache block of a channel: serving repeated identical sync requests from a stored response, the cache key, key_logic, TTL, invalidation namespaces and the backing store. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache`

`cache` serves repeated identical requests from a stored response instead of executing the workflow. It applies to the synchronous HTTP ingress only.

## Synopsis

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `enabled` | boolean | yes | — | `false` disables the cache without removing the block. |
| `ttl_secs` | integer | no | `300` | Seconds an entry lives. |
| `cache_key_fields` | array of strings | no | whole payload | Payload fields that form the cache key. |
| `key_logic` | JSONLogic | no | — | Computes the cache key over `{data, metadata}`. Takes precedence over `cache_key_fields`. |
| `connector` | string | no | in-memory | Name of a [cache connector](../connectors/index.md) backing the cache. In cluster mode the default is the shared cluster Redis. |
| `namespaces` | array of strings | no | — | 1 to 8 [invalidation namespaces](#invalidation) the channel's entries belong to. Each name is 1–64 of `a-z 0-9 _ - . :`. |
| `coalesce_misses` | boolean | no | `false` | [Coalesce concurrent misses](#coalescing-misses) for one key on this node, so an expiry under load costs one workflow run. |

**The cache key** is derived from exactly these parts: the channel name, the HTTP method, the route parameters, the query string, and the request payload. Parameters and query string are order-independent. The payload part is the whole payload, the subset named by `cache_key_fields`, or the result of `key_logic`.

`key_logic` is the general form. It reads `{"data": …, "metadata": …}`, the same metadata `validation_logic` reads, and **replaces** the payload-derived half of the key rather than adding to it. An expression that says what varies the response is a complete answer. Mixing it with a payload hash would put back the fields it was written to exclude. When both are declared, `key_logic` decides the key and `cache_key_fields` is ignored.

A per-user cache keys on the verified subject. On a [`jwt`](./auth.md) channel the claims are at `metadata.auth.claims`, and a caller cannot supply that key, so a request without a verified token resolves the expression to `null` and bypasses the cache:

```json
"cache": {
  "enabled": true,
  "ttl_secs": 60,
  "key_logic": { "cat": [{ "var": "metadata.auth.claims.sub" }, "|", { "var": "data.report_id" }] }
}
```

An expression that does not compile quarantines the channel rather than falling back. A cache key that silently widens serves one caller's body to the next. One that resolves to `null` at request time bypasses the cache for that request, as an unresolvable `cache_key_fields` does. Each entry resolves as a literal payload key (`user_id`), a dotted path (`user.id`), or the same path with a leading `data.` prefix (`data.user_id`). A request that resolves **none** of the declared fields bypasses the cache entirely. The workflow runs, nothing is stored, and Orion logs a warning naming the channel and fields. That almost always means the names do not match the payload shape.

> [!WARNING]
> Request headers are not part of the cache key unless `key_logic` reads them from `metadata.headers`. Without that, a cached entry is shared by every caller whose method, route, query, and payload agree, whatever headers they sent. Credential headers are masked there, so a response that varies by caller must key on the verified claims, not on a token.

Behaviour:

- A hit is served without executing the workflow and without consuming a backpressure permit.
- A replayed idempotency key answers `409` before the cache is consulted.
- The response is stored on success and expires after `ttl_secs`.
- A cached [shaped](./response.md) response replays its status and headers, not only its body.
- A write-gated cache connector is refused for the response cache; see [operation gates](../connectors/index.md).

## Invalidation

A TTL alone trades staleness against misses: a short one misses while the data sits unchanged, and a long one serves stale data after every write. `namespaces` lets a channel cache until the data changes:

```json
"cache": { "enabled": true, "ttl_secs": 3600, "namespaces": ["ladder"] }
```

Each namespace has a version counter in the response-cache store. An entry records the versions current when its request looked the cache up, and it is served only while they are still current. Invalidating a namespace bumps its counter, so every entry stored under an older version stops matching, on every channel that declares the namespace. There is no scan and no delete. A stale entry is overwritten by the next miss, or expires at its TTL. Invalidate from either side:

- A workflow task: [`cache_invalidate`](../functions/cache_invalidate.md) with `{"namespaces": ["ladder"]}`, in the workflow that makes the write.
- An operator: [`POST /api/v1/admin/cache/namespaces/ladder/invalidate`](../admin-api/cache.md), for a change no workflow made.

Behaviour:

- **One round trip.** The versions are read in the same `MGET` as the entry, so a namespaced lookup costs what a plain one does.
- **No stale store.** An entry is stored with the versions read at its lookup, before the workflow ran. An invalidation that lands while the workflow is running makes the entry it produces stale at once, rather than pinning a response computed from the old data.
- **`ttl_secs` is the ceiling.** An invalidation that fails to reach a store (an unreachable Redis, say) leaves entries there to expire at their TTL, so the TTL bounds the staleness even then.
- **Every store.** An invalidation bumps the namespace in the default store, in every in-memory response-cache store on the node, and in every Redis cache connector. A store it cannot reach is logged and skipped. A channel archived with live entries and reactivated later therefore reads them against an already-bumped counter.
- **The counter's key** is `orion:rc:ns:<namespace>`, in each store. `INCR` on it from outside Orion is an invalidation too.
- A lookup that cannot read the counters, or finds one holding something other than an integer, bypasses the cache for that request and stores nothing.

A namespaced entry is stored under a key prefix of its own, so an older binary never reads one as a response body. An older binary also refuses the `namespaces` field and quarantines the channel, so during a rolling upgrade such a channel is served only by nodes that understand it.

## Coalescing misses

When an entry expires under load, every request that arrives before the refill misses, and each one runs the workflow and its queries. `coalesce_misses: true` makes that one run per key on each node:

```json
"cache": { "enabled": true, "ttl_secs": 30, "coalesce_misses": true }
```

- The first request to miss a key runs the workflow. Requests that miss the same key while it runs wait, then are served the entry it stores. They count in `orion_response_cache_coalesced_total`.
- A waiting request holds no [backpressure](./backpressure.md) permit, because it does no work.
- The wait is bounded by the channel's `timeout_ms`, capped at 5 seconds. A request that waits that long runs the workflow itself.
- A first run that stores nothing, because its workflow failed or its response carried task errors, releases the waiting requests at once, and each runs the workflow itself.
- Coalescing is per node. Replicas sharing a Redis cache each run one workflow per key, not one between them.

It combines with `namespaces`: a request waiting on a run that an invalidation overtook finds that run's entry already stale, and runs the workflow itself.

**Cluster mode.** With the shared cluster Redis, hits are shared across replicas. A channel whose cache connector is missing, broken, or explicitly in-memory refuses to load. On a single node it falls back to process memory with a warning.

## Related

- [Connector types › Cache](../connectors/cache.md): the connector that backs the cache.
- [Configure a channel](../../guides/author/channels.md): adding a cache in practice.
- [`rate_limit`](./rate_limit.md): the same `key_logic` vocabulary.
- [Deploy a cluster](../../operate/deploy/cluster.md): the shared cache a cluster requires.
- [Channel configuration](./index.md): every key, with its page.
