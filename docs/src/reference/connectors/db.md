<!-- description: The `db` connector config: the connection string, pool bounds, per-operation gates and the dialect guards that bound which tables it reaches. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# `db` connectors

The `config` fields of a `db` connector, which backs PostgreSQL, MySQL and SQLite.

Runs parameterized queries against PostgreSQL, MySQL or SQLite. The `connection_string` scheme selects the backend: `postgres://` or `postgresql://`, `mysql://` or `mariadb://`, `sqlite:`. Any other scheme, `mongodb://` included, is refused when the set is linted or compiled.

```json
{
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "driver": "postgresql",
    "connection_string": "env://ORDERS_DB_URL",
    "max_connections": 10,
    "connect_timeout_ms": 5000,
    "query_timeout_ms": 30000
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connection_string` | string | yes | — | Database URL; the scheme selects the backend. Carries credentials, so write it as a secret reference |
| `driver` | string | in a definition set | — | `postgresql`, `mysql` or `sqlite`: the backend, stated. See [The `driver` field](#the-driver-field) |
| `max_connections` | integer | no | — | Connection pool maximum size |
| `connect_timeout_ms` | integer | no | — | Connection establishment timeout |
| `query_timeout_ms` | integer | no | — | Per-query timeout |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection). Ignored for `sqlite:`, which opens a file |
| `operations` | object | no | all allowed | `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` — see [Operation gates](./operation-gates.md) |
| `dialect` | object | no | both guards off | [Dialect guards](#dialect-guards) |

There are two ways to talk to it. The portable [`data_query` / `data_write`](../data-dialect.md) dialect runs unchanged against every SQL backend. The other is raw SQL through [`db_read` / `db_write`](../functions/db_read.md).

There is no `retry` field: a statement that timed out may already have been applied, so database calls are never re-driven. See [Retries](./reliability.md#retries). Bound the call with `connect_timeout_ms` and `query_timeout_ms` instead.

## The `driver` field

`driver` names the backend the connection string selects. It does not select it: the scheme still does. Where the scheme is visible, the two must agree. `"driver": "mysql"` with a `postgres://` URL is refused at upload and by `lint`. A connection string that is a secret reference (`env://…`) or a `${VAR}` placeholder shows no scheme until the serving node resolves it, so there is nothing to compare, and the field is the only statement of the backend. Any value other than the three names is refused.

A [definition set](../cli/orion-cli/lint.md) requires it: `lint <dir>` and `compile` refuse a `db` connector without one with `[connector.db_driver]`, whether its connection string is a reference or not. A set is what `orion-cli compile` reads, and a deployed connection's provider is its driver (`org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite`), which a hidden scheme cannot name. The field becomes the provider and is not carried in the connection's `public_config`.

## Dialect guards

`db` connectors carry a `dialect` block that bounds what the portable dialect may reach. [Operation gates](./operation-gates.md) answer *which verbs*; dialect guards answer *which tables*. Both guards default to off.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `require_schema` | boolean | no | `false` | Refuse any `data_query` / `data_write` call without a real [schema](../data-dialect.md): at least one declared entity, and not `"unmapped": "identity"` |
| `allowed_entities` | array of strings | no | `[]` (unrestricted) | Physical table names the dialect may touch. Matched after schema renames apply; covers relation targets and junction tables |

Unknown keys inside `dialect` are refused.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Portable data dialect](../data-dialect.md): the language `data_query` and `data_write` speak.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
