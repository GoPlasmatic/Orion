<!-- description: The `db` connector config: the connection string, pool bounds, per-operation gates and the dialect guards that bound which tables it reaches. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `db` connectors

The `config` fields of a `db` connector, which backs PostgreSQL, MySQL and SQLite.

Runs parameterized queries against PostgreSQL, MySQL or SQLite. The `connection_string` scheme selects the backend: `postgres://`, `mysql://` or `sqlite:`. Any other scheme, `mongodb://` included, is refused at create and update. There is no `driver` field.

```json
{
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "env://ORDERS_DB_URL",
    "max_connections": 10,
    "connect_timeout_ms": 5000,
    "query_timeout_ms": 30000
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connection_string` | string | yes | — | Database URL; the scheme selects the backend. Carries credentials, and is always masked in API reads |
| `max_connections` | integer | no | — | Connection pool maximum size |
| `connect_timeout_ms` | integer | no | — | Connection establishment timeout |
| `query_timeout_ms` | integer | no | — | Per-query timeout |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection). Ignored for `sqlite:`, which opens a file |
| `operations` | object | no | all allowed | `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` — see [Operation gates](./operation-gates.md) |
| `dialect` | object | no | both guards off | [Dialect guards](#dialect-guards) |

There are two ways to talk to it. The portable [`data_query` / `data_write`](../data-dialect.md) dialect runs unchanged against every SQL backend. The other is raw SQL through [`db_read` / `db_write`](../functions/db_read.md).

There is no `retry` field: a statement that timed out may already have been applied, so database calls are never re-driven. See [Retries](./reliability.md#retries). Bound the call with `connect_timeout_ms` and `query_timeout_ms` instead.

## Dialect guards

`db` connectors carry a `dialect` block that bounds what the portable dialect may reach. [Operation gates](./operation-gates.md) answer *which verbs*; dialect guards answer *which tables*. Both guards default to off.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `require_schema` | boolean | no | `false` | Refuse any `data_query` / `data_write` call without a real [schema](../data-dialect.md): at least one declared entity, and not `"unmapped": "identity"` |
| `allowed_entities` | array of strings | no | `[]` (unrestricted) | Physical table names the dialect may touch. Matched after schema renames apply; covers relation targets and junction tables |

Unknown keys inside `dialect` are refused on create and update.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Portable data dialect](../data-dialect.md): the language `data_query` and `data_write` speak.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
