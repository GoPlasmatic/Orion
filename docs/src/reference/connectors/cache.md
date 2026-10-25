<!-- description: The `cache` connector config: the per-node memory backend, and the read and write operation gates. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# `cache` connectors

The `config` fields of a `cache` connector, a named in-process memory store.

Key-value storage for lookups, session state, counters and temporary data, through [`cache_read`](../functions/cache_read.md), [`cache_write`](../functions/cache_write.md), [`cache_delete`](../functions/cache_delete.md) and [`cache_incr`](../functions/cache_incr.md).

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "memory"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `backend` | string | yes | — | `"memory"`, the only backend. Any other value is refused |
| `operations` | object | no | all allowed | `read` / `write` — see [Operation gates](./operation-gates.md). `write` also gates `cache_delete` and `cache_incr` |

TTL is set per write, through `cache_write`'s `ttl_secs`. There is no connector-level default.

## Per-node memory

A cache connector is memory in the process that serves the request. Each node has its own copy, and a restart empties it. Two nodes behind a load balancer each see only their own writes. The store is bounded by [`engine.max_memory_cache_entries`](../configuration/engine.md), and each connector gets its own budget, separate from the channel stores.

A connector has no endpoint, so a `url` is refused. Exports from earlier releases carry `"url": null` and `"allow_private_urls": false`, and those still import.

A channel's [deduplication](../channel-config/deduplication.md) store and [response cache](../channel-config/cache.md) can name a cache connector. In cluster mode they must not. A dedup store or response cache shared across nodes uses the cluster's shared store, the state database, when the channel names **no** connector. A channel that names an in-memory connector in cluster mode refuses to load rather than degrade to per-node state.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
