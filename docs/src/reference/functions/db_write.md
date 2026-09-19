<!-- description: The db_write task function: run a raw INSERT, UPDATE or DELETE with bound parameters against a SQL connector and report the rows affected. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `db_write`

The raw-SQL escape hatch for writes (multi-table statements, `UPDATE … FROM`,
SQL functions in `SET`, DDL). Runs an `INSERT`/`UPDATE`/`DELETE` against a SQL
connector and writes `{ "rows_affected": N }`. Note: the author writes
dialect-specific SQL, and a connector can disable this function entirely through its [`raw_write` operation gate](../data-dialect.md#connector-operation-gates).

## Synopsis

```json
{
  "name": "db_write",
  "input": {
    "connector": "primary-db",
    "query": "INSERT INTO orders (id, total) VALUES (?, ?)",
    "params": [
      {
        "var": "data.order.id"
      },
      {
        "var": "data.order.total"
      }
    ],
    "output": "data.write_result"
  }
}
```

## Description

`db_write` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `depends_on` `sql`. See [Retry safety](./retry-safety.md) for what the answer costs.

An `INSERT` also carries `last_insert_id` on MySQL and SQLite, the same key
[`data_write`](./data_write.md) reports. PostgreSQL does not report one; it uses `RETURNING`, which the portable dialect supports. The key appears for an `INSERT`/`REPLACE` only. SQLite's `last_insert_rowid` belongs to the *connection*, so after an `UPDATE` it would report whatever an earlier insert
on that pooled connection left behind.

**The statement can live in a file.** A long statement is easier to review as SQL than as one JSON string: write `"query": {"$sql": "sql/settle.sql"}` and keep comments and indentation in the file. [`orion-server compile`](../cli/orion-server/compile.md) inlines it in normal form, so the server only ever sees the string. See [Statements in `.sql` files](../cli/shared-definitions.md#statements-in-sql-files).

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `INSERT`/`UPDATE`/`DELETE` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order. Each element folds `{"var": …}` and nothing else — an operator written inline binds as a literal object, which `lint` reports as `logic.unresolvable`; compute it in a `map` task first |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where `{ "rows_affected": N }` (plus `last_insert_id` after an insert) is written |

## Examples

```json
{
  "name": "db_write",
  "input": {
    "connector": "primary-db",
    "query": "INSERT INTO orders (id, total) VALUES (?, ?)",
    "params": [{ "var": "data.order.id" }, { "var": "data.order.total" }],
    "output": "data.write_result"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect › Connector operation gates](../data-dialect.md#connector-operation-gates): the `raw_write` gate that can disable this function.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
