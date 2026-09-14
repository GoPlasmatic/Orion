<!-- description: The data_write task function: a backend-neutral insert, update, delete or upsert rendered natively for SQL, MongoDB or Elasticsearch connectors. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `data_write`

The write counterpart of `data_query`: one **backend-neutral mutation** —
`insert`, `update`, `delete`, or `upsert` — rendered natively for SQL, MongoDB,
or Elasticsearch. The `filter` of an update/delete is the query dialect's
filter, unchanged. See the [Portable Data Dialect](../data-dialect.md) reference
for the full envelope, backend mapping, and safety rules.

## Synopsis

```json
{
  "name": "data_write",
  "input": {
    "connector": "orders-db",
    "write": {},
    "params": {
      "id": {
        "var": "data.order_id"
      }
    },
    "schema": {},
    "database": "…",
    "numeric_as": "number",
    "binary_as": "auto",
    "output": "data.write_result"
  }
}
```

## Description

`data_write` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `depends_on` `op`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of a `db` or `es` connector |
| `write` | object | yes | — | The mutation envelope — fields below |
| `params` | object | no | `{}` | Named values referenced as `{ "param": "name" }` inside `values`, `set`, and `filter`; each value is JSONLogic resolved against the context |
| `schema` | object | yes | — | Inline entity schema: renames, allowlist, `writable` flags. Undeclared entities and columns are rejected; `{"unmapped": "identity"}` accepts undeclared names as physical ones. Enforced at run time, like `data_query`'s |
| `database` | string | conditional | — | Database name; required when the connector is MongoDB (checked at workflow activation), unused otherwise |
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](./db_read.md#decimal-columns). SQL backends only |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](./db_read.md#binary-columns). SQL backends only |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the write result is written |

Inside `write`:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `op` | string | yes | — | `insert` \| `update` \| `delete` \| `upsert` |
| `target` | string | yes | — | Logical entity → table / collection / index |
| `values` | object \| array | conditional | — | The row object or objects to insert; required for `insert` and `upsert` |
| `set` | object | conditional | — | Column → value/param assignments; required for `update`, optional overrides on `upsert` conflict |
| `filter` | JSONLogic | conditional | — | Row selection for `update`/`delete` (same operators as `data_query`); required unless the unfiltered opt-in below is used |
| `on_conflict` | object | conditional | — | `{ "target": [cols], "action": "update" \| "nothing" }`; required for `upsert` |
| `returning` | array | no | — | Columns returned from mutated rows (PostgreSQL/SQLite only) |
| `all` | bool | no | `false` | Acknowledge an intentionally unfiltered update/delete |

## Examples

```json
{
  "name": "data_write",
  "input": {
    "connector": "orders-db",
    "params": { "id": { "var": "data.order_id" } },
    "schema": {
      "entities": {
        "orders": {
          "columns": {
            "id": { "type": "int", "writable": false }, "status": { "type": "text" }
          }
        }
      }
    },
    "output": "data.write_result",
    "write": {
      "op": "update",
      "target": "orders",
      "set": { "status": "shipped" },
      "filter": { "==": [{ "field": "id" }, { "param": "id" }] }
    }
  }
}
```

Safety guards: unfiltered mutations are rejected unless `"all": true` **and** `write.allow_unfiltered` are both set, and bulk inserts over `write.max_rows` are rejected. A connector's [operation gates](../data-dialect.md#connector-operation-gates) can disable individual ops entirely. Results are normalized per backend. SQL returns `{ "status": "ok", "rows_affected": n }`, plus `returning` / `last_insert_id` where supported. MongoDB and Elasticsearch return doc-store counts (`inserted`/`ids`, `matched`/`modified`, `deleted`). Every result carries a `status`; a bulk insert that applied only some of its rows reports `"partial"` with a per-item array. See [Bulk writes](../data-dialect.md#bulk-writes).

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect › Write envelope](../data-dialect.md#write-envelope-data_write): the mutation envelope, safety guards and result shapes.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
