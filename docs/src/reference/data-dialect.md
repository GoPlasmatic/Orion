# Portable Data Dialect

The **portable data dialect** lets a workflow express a database query or
mutation once — in a backend-neutral, JSONLogic-shaped envelope — and run it
unchanged against **PostgreSQL, MySQL, SQLite, MongoDB, or Elasticsearch**.
Switching backend is a connector change, not a rewrite.

Two functions implement it:

- [`data_query`](./functions.md#data_query) — reads: filter, project, sort,
  paginate, and include related records.
- [`data_write`](./functions.md#data_write) — writes: insert, update, delete,
  and upsert.

The raw functions (`db_read`, `db_write`, `mongo_read`) remain the **escape
hatch** for anything outside the portable vocabulary — hand-written SQL, CTEs,
multi-table statements, aggregations.

```json
{
  "name": "data_query",
  "input": {
    "connector": "orders_db",
    "query": {
      "source": "users",
      "filter": { "and": [
        { ">":  [{ "field": "age" }, 25] },
        { "in": [{ "field": "status" }, ["active", "trial"]] }
      ] },
      "fields": ["id", "name", "age"],
      "sort": [{ "age": "asc" }],
      "limit": 50
    },
    "output": "data.users"
  }
}
```

Point `connector` at Postgres and this renders parameterized SQL via sea-query;
at MongoDB and it renders a `find` filter document; at Elasticsearch and it
renders a Query DSL search body. The results come back as the same JSON row
array either way.

## The token model

Three tokens appear inside envelopes, and they never collide:

| Token | Meaning | Valid in |
|-------|---------|----------|
| `{ "field": "name" }` | A column / document field | `filter` (and as keys in `values`/`set`) |
| `{ "param": "p" }` | A named value from the `params` map, resolved **before** rendering | `filter`, `values`, `set` |
| `{ "var": "data.x" }` | An ordinary datalogic context lookup | only inside the `params` map |

`params` is the single point where the message touches a query or mutation — it
produces literal values, never SQL or query text:

```json
{
  "filter": { "==": [{ "field": "id" }, { "param": "id" }] },
  "params": { "id": { "var": "data.req.id" } }
}
```

Every resolved value is a **bound parameter** (SQL) or a document value
(Mongo/ES). No user value is ever string-interpolated — the dialect is
injection-safe by construction.

## Query envelope (`data_query`)

| Field | Type | Description |
|-------|------|-------------|
| `source` | string | Logical entity → table / collection / index (schema-resolved) |
| `filter` | JSONLogic | Condition from the operator vocabulary below; omit for "match all" |
| `fields` | array | Projection; omit for all columns/fields |
| `sort` | array | `[{ "age": "asc" }, { "name": "desc" }]` — deterministic null ordering across backends |
| `limit` / `skip` | number | Pagination. Missing `limit` gets `query.default_limit`; above `query.max_limit` is **rejected, never clamped** |
| `include` | array | Nested related records, hydrated per relation (see [Relations](#relations-and-includes)) |

### Operator vocabulary

| Operator | Meaning |
|----------|---------|
| `and`, `or`, `!` | Boolean combinators |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | Comparisons (`===`/`!==` are aliases). `{"==": [x, null]}` is a null test |
| `<`/`<=` (ternary) | Range: `{ "<=": [1, { "field": "x" }, 10] }` → BETWEEN |
| `in` | Membership (list haystack) or substring containment (string haystack) |
| `starts_with`, `ends_with` | Text anchors (rendered as `LIKE` / `$regex` / `prefix`+`wildcard`) |
| `missing` | Field(s) have no meaningful value |
| `some`, `all`, `none` | Quantifiers over a declared relation |

Anything outside this vocabulary is rejected with a **located error** naming
the operator and position — never silently ignored.

## Write envelope (`data_write`)

| Field | Used by | Description |
|-------|---------|-------------|
| `op` | all | `insert` \| `update` \| `delete` \| `upsert` |
| `target` | all | Logical entity → table / collection / index |
| `values` | insert, upsert | A row object, or an array of rows (bulk) |
| `set` | update, upsert | Column → value/param assignments |
| `filter` | update, delete | Row selection — **the query dialect's filter**, same operators, same rendering |
| `on_conflict` | upsert | `{ "target": ["email"], "action": "update" \| "nothing" }` |
| `returning` | all | Columns to return from mutated rows (capability-gated, see below) |
| `all` | update, delete | Explicit acknowledgement for an intentionally unfiltered mutation |

One task per operation:

```json
{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "insert", "target": "users",
    "values": [ { "name": "Ada", "status": "active" }, { "name": "Grace", "status": "active" } ],
    "returning": ["id"], "output": "data.created" } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "update", "target": "users",
    "set": { "status": "inactive" },
    "filter": { "==": [{ "field": "id" }, { "param": "id" }] },
    "params": { "id": { "var": "data.req.id" } },
    "output": "data.updated" } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "upsert", "target": "users",
    "values": { "email": "ada@x.io", "name": "Ada" },
    "on_conflict": { "target": ["email"], "action": "update" },
    "output": "data.upserted" } }
```

### Safety guards

- **Unfiltered mutations are rejected.** An `update`/`delete` with no `filter`
  would rewrite or truncate the whole table. It fails unless the envelope
  carries `"all": true` **and** the server enables `write.allow_unfiltered`
  (default `false`) — a deliberate double opt-in.
- **Bulk inserts are capped.** A `values` array longer than `write.max_rows`
  (default 1000) is rejected — never silently truncated.
- **Connector operation gates.** A connector's config can disable operation
  types entirely (`operations: { "delete": false }`) — see
  [Connector operation gates](#connector-operation-gates).

## Backend mapping

| `op` | SQL (sea-query) | MongoDB | Elasticsearch |
|------|-----------------|---------|---------------|
| query | `SELECT … WHERE …` | `find(filter)` | `POST {index}/_search` |
| `insert` | `INSERT INTO … VALUES …` | `insertOne` / `insertMany` | `POST {index}/_bulk` |
| `update` | `UPDATE … SET … WHERE …` | `updateMany(filter, {$set})` | `POST {index}/_update_by_query` (painless script) |
| `delete` | `DELETE FROM … WHERE …` | `deleteMany(filter)` | `POST {index}/_delete_by_query` |
| `upsert` | `ON CONFLICT … DO UPDATE / DO NOTHING` (PG/SQLite); `ON DUPLICATE KEY UPDATE` (MySQL) | `updateOne(…, upsert: true)` | `POST {index}/_update/{id}` (`doc_as_upsert`); `op_type=create` for `nothing` |

The `filter` of an update/delete goes through the exact same rendering path as
a query's `WHERE` — including relation predicates (`some` → SQL `EXISTS`).

### Parity or error

The dialect's governing rule: **match the reference semantics where a backend
can; raise a precise, located capability error where it cannot; never
approximate silently.** The notable divergences:

| Feature | Behavior |
|---------|----------|
| `returning` | Native on PostgreSQL/SQLite. On MySQL it is rejected (`FeatureUnsupportedByTarget`); single-row inserts report `last_insert_id` instead. On MongoDB inserts report generated `ids`. On Elasticsearch it is rejected; inserts report `ids` |
| `all` over an ES relation | Rejected (not set-equivalent on nested documents) |
| Deep ES pagination | `skip + limit` beyond `max_result_window` (10k) is rejected, not truncated |
| Bulk upsert on Mongo/ES | Rejected — single-row upserts only |
| ES upsert conflict target | Must resolve to the document `_id` (declare a schema rename); anything else is rejected |

### Elasticsearch notes

- **`_id` is explicit.** ES keys documents on the metadata `_id`, which lives
  outside the document source. There is no implicit `id` → `_id` mapping —
  declare it in the schema (`{"columns": {"id": {"name": "_id"}}}`). A physical
  `_id` column is then lifted into the bulk action / URL path on insert and
  upsert; mutating `_id` in `set` is rejected.
- **Read-your-writes.** Every ES write requests a refresh (`wait_for`, or
  `true` on the by-query endpoints), so a `data_query` later in the same
  pipeline sees the write — parity with SQL/Mongo visibility, at a throughput
  cost.
- **`_bulk` is ordered but non-transactional.** Any item error fails the call,
  though earlier items may already be applied. Version conflicts on
  `_update_by_query`/`_delete_by_query` surface as errors (`conflicts=abort`).

## Result shapes

Written to the task's `output` path:

| Backend | Shape |
|---------|-------|
| Query (all) | JSON array of rows/documents |
| SQL write | `{ "rows_affected": n }`, plus `"returning": [..]` where supported and `"last_insert_id": n` on MySQL single-row inserts |
| MongoDB / ES write | Doc-store keys per op: `{ "inserted": n, "ids": [..] }`, `{ "matched": n, "modified": n }` (+ `"upserted_id"` when created), `{ "deleted": n }` |

## The schema registry

Both functions accept an optional inline `schema` — **privileged configuration
authored alongside the workflow, never built from request input**. Without one,
the dialect runs in *identity mode*: names pass through as-is.

```json
"schema": {
  "entities": {
    "users": {
      "table": "app_users",
      "columns": {
        "id":     { "name": "user_id", "type": "int" },
        "email":  { "type": "string" },
        "secret": { "queryable": false, "writable": false }
      },
      "relations": {
        "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "user_id" }
      }
    }
  },
  "unmapped": "reject"
}
```

- **Renames** — logical entity/column names map to physical tables/columns
  (`id` → `user_id`, or `id` → `_id` for Elasticsearch).
- **Types** — drive value coercion where a backend needs the hint.
- **Allowlist** — with `"unmapped": "reject"`, only declared entities/columns
  are usable; `queryable: false` hides a column from reads, `writable: false`
  protects it from writes (generated/identity columns).
- **Relations** — declare `has_many` / `belongs_to` / many-to-many (via
  `through`) so `some`/`all`/`none` predicates and `include` work.

## Relations and includes

With relations declared, a filter can quantify over related records:

```json
{ "some": [{ "field": "orders" }, { ">": [{ "field": "total" }, 100] }] }
```

renders as a correlated `EXISTS` (SQL), `$elemMatch` over embedded documents
(Mongo), or a `nested`/`has_child` query (ES). `include` fetches the related records
themselves, hydrated with one child query per relation:

```json
"include": [ { "relation": "orders", "fields": ["id", "total"], "limit": 10 } ]
```

## Connector operation gates

A `db` or `es` connector's config can en/disable operation types, regardless
of what workflows ask for. Everything defaults to allowed:

```json
{
  "name": "orders-db-readonly",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://…",
    "operations": {
      "read": true,
      "insert": true,
      "update": false,
      "delete": false,
      "upsert": false,
      "raw_write": false
    }
  }
}
```

| Gate | Blocks |
|------|--------|
| `read` | `data_query`, `db_read`, `mongo_read` |
| `insert` / `update` / `delete` / `upsert` | the matching `data_write` op |
| `raw_write` | the raw-SQL `db_write` escape hatch |

A gated call fails with a validation error naming the operation and connector
(`operation 'delete' is disabled on connector 'orders-db-readonly'`). Because
raw SQL cannot be classified per-statement, `db_write` has its own `raw_write`
gate — to make a connector fully delete-proof, disable both `delete` and
`raw_write`.

## Configuration

```toml
[query]                    # Page-size bounds for data_query
# default_limit = 100      # Page size when a query omits `limit`
# max_limit = 1000         # Hard cap; a query asking for more is rejected

[write]                    # Safety bounds for data_write
# max_rows = 1000          # Cap on rows per bulk insert/upsert (over → rejected)
# allow_unfiltered = false # Permit unfiltered update/delete (still needs per-call "all": true)
```

Both sections are overridable via environment variables
(`ORION_QUERY__MAX_LIMIT`, `ORION_WRITE__MAX_ROWS`, …).
