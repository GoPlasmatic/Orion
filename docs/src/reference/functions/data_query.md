<!-- description: The data_query task function: one backend-neutral query envelope run against a SQL, MongoDB or Elasticsearch connector through the portable dialect. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `data_query`

Runs one **backend-neutral query** against a SQL (PostgreSQL, MySQL, SQLite), MongoDB, or Elasticsearch connector. The connector decides the rendering: parameterized SQL through sea-query, a Mongo `find`, or an ES `_search` body. The full envelope, operator vocabulary, schema registry, and relation support are in the [Portable data dialect](../data-dialect.md) reference.

## Synopsis

```json
{
  "name": "data_query",
  "input": {
    "connector": "orders-db",
    "query": {},
    "params": {
      "cid": {
        "var": "data.customer_id"
      }
    },
    "schema": {},
    "database": "…",
    "numeric_as": "number",
    "binary_as": "auto",
    "output": "data.orders"
  }
}
```

## Description

`data_query` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of a `db` or `es` connector |
| `query` | object | yes | — | The query envelope: `source`, `filter`, `fields`, `sort`, `limit`, `skip`, `after`, `include`, `count` — see the [query envelope](../data-dialect.md#query-envelope-data_query) |
| `params` | object | no | `{}` | Named values referenced as `{ "param": "name" }` inside the filter; each value is JSONLogic resolved against the context |
| `schema` | object | yes | — | Inline entity schema: renames, types, allowlist, relations. Undeclared entities and columns are rejected; `{"unmapped": "identity"}` accepts undeclared names as physical ones |
| `database` | string | conditional | — | Database name; required when the connector is MongoDB (checked at workflow activation), unused otherwise |
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](./db_read.md#decimal-columns). SQL backends only |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](./db_read.md#binary-columns). SQL backends only |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the row array is written |

> [!NOTE]
> The `schema` requirement is enforced when the query runs, not when the
> workflow is created: a task without one is accepted at create and refused at
> its first request, with an error naming the key to add. Every entity the
> dialect resolves goes through the schema, so no schema-less call can succeed.

## Examples

```json
{
  "name": "data_query",
  "input": {
    "connector": "orders-db",
    "query": {
      "source": "orders",
      "filter": { "and": [
        { "==": [{ "field": "customer_id" }, { "param": "cid" }] },
        { ">":  [{ "field": "total" }, 100] }
      ] },
      "sort": [{ "created_at": "desc" }, { "id": "asc" }],
      "limit": 20
    },
    "params": { "cid": { "var": "data.customer_id" } },
    "schema": {
      "entities": {
        "orders": {
          "columns": {
            "id": { "type": "int" }, "customer_id": { "type": "int" },
            "total": { "type": "float" }, "created_at": { "type": "timestamp" }
          }
        }
      }
    },
    "output": "data.orders"
  }
}
```

Page sizes are bounded by the [`[query]` config section](../configuration/index.md) (`default_limit` / `max_limit`), and `skip` by `max_skip`. A query asking for more than a cap is rejected, never clamped. To read past `max_skip`, or to page a list whose rows move, use the `after` cursor; see [Paging](../data-dialect.md#paging).

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect](../data-dialect.md): the envelope, operators, paging and parity rules behind this function.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
