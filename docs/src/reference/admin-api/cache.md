<!-- description: The response-cache endpoint: invalidating a cache namespace, so every channel declaring it misses on its next request, on every node sharing the store. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# Cache endpoints

Invalidating a channel response-cache namespace by hand.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/cache/namespaces/{namespace}/invalidate` | Bump the namespace's version in every response-cache store. Every channel whose [`cache.namespaces`](../channel-config/cache.md#invalidation) lists it misses on its next request |

It is the operator's form of the [`cache_invalidate`](../functions/cache_invalidate.md) function. Use it for a change no workflow made, such as a board flipped by hand or a row fixed in the database.

```bash
curl -X POST http://localhost:8080/api/v1/admin/cache/namespaces/ladder/invalidate \
  -H "x-api-key: $ORION_API_KEY"
```

```json
{ "data": { "namespace": "ladder", "stores": 2 } }
```

`stores` counts what the bump reached: the default response-cache store, each in-memory store on the node that answered, and each Redis cache connector. On a cluster the default store is the shared Redis, so one call reaches every node. A name outside `a-z 0-9 _ - . :`, or longer than 64 characters, is a `400`. The call writes an `invalidate` / `cache_namespace` [audit row](../../operate/run/audit-logs.md).

## Related

- [`cache`](../channel-config/cache.md#invalidation): declaring namespaces on a channel.
- [`cache_invalidate`](../functions/cache_invalidate.md): the same bump from a workflow.
- [Operational endpoints](./operations.md): the other endpoints that drive a running instance.
