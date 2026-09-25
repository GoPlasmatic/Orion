<!-- description: The operations block every connector type carries: which gate blocks which task function, the http method allow-list, and cache stores. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Operation gates

The `operations` block that limits what workflows may do through a connector, per type.

Every connector type carries an `operations` block that limits what workflows may do through it. Every gate defaults to allowed. A disabled operation turns the call into a validation error naming the operation and the connector — regardless of what any workflow asks for.

| Type | Gate | Blocks |
|------|------|--------|
| `db`, `es` | `read` | `data_query`, `db_read`, `mongo_read`, `mongo_aggregate` |
| `db`, `es` | `insert`, `update`, `delete`, `upsert` | The matching `data_write` operation, and the matching `mongo_write` op (`insert_*` → `insert`, `update_*`/`replace_one` → `update` — or `upsert` when `"upsert": true` — `delete_*` → `delete`) |
| `db`, `es` | `raw_write` | `db_write` — raw SQL cannot be classified per operation |
| `cache` | `read` | `cache_read` |
| `cache` | `write` | `cache_write`, `cache_delete`, `cache_incr`, plus channel stores backed by the connector — see [Cache write covers channel stores](#cache-write-covers-channel-stores) |
| `kafka` | `publish` | `publish_kafka` |
| `http` | `methods` | Any method not on the allow-list — see [http gates by method](#http-gates-by-method) |
| `storage` | `presign_get`, `presign_put`, `head` | The matching storage function/method |

To make a `db` connector fully delete-proof, disable both `delete` and `raw_write`:

```json
{
  "type": "db",
  "connection_string": "env://ORDERS_DB_URL",
  "operations": { "delete": false, "raw_write": false }
}
```

## `http` gates by method

An HTTP connector's operation *is* its method, so the gate is an allow-list. Empty — the default — allows every method `http_call` can issue. Naming even one method makes the list exhaustive: `{ "methods": ["GET"] }` locks the connector to reads. Matching ignores case. An entry outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE` is refused on create and update.

## Cache `write` covers channel stores

A channel's [deduplication store and response cache](../channel-config/index.md) may name a cache connector, and both write through it. A write-gated connector is refused for those uses. In cluster mode the channel fails to load and says why; on a single node it falls back to process memory with a warning. `read` does not apply to them — the only keys either store reads back are ones Orion wrote. There is no separate cache `delete` gate. `cache_delete` changes what later reads see exactly as `cache_write` does, so `write` covers it and a read-only connector refuses both.

An `operations` key the connector's type does not have is refused on create and update, naming the key and listing the ones that exist.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [`db`](./db.md): the dialect guards that answer *which tables* rather than which verbs.
- [Channel configuration](../channel-config/index.md): the channel stores a cache `write` gate covers.
