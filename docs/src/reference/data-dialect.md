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
| `sort` | array | `[{ "age": "asc" }, { "name": "desc" }]`. **A null (or missing field) sorts as the smallest value** — nulls first on `asc`, last on `desc` — on every backend |
| `limit` / `skip` | number | Pagination. Missing `limit` gets `query.default_limit`; a `limit` above `query.max_limit` or a `skip` above `query.max_skip` is **rejected, never clamped** |
| `include` | object | Relation name → `{ "fields": [..], "sort": [..], "limit": n }`; nested related records, hydrated per relation (see [Relations](#relations-and-includes)) |

The null-ordering rule is the one every backend can state natively: it *is* the
default order of SQLite, MySQL and MongoDB, and PostgreSQL and Elasticsearch are
given an explicit `NULLS FIRST` / `missing` clause to match. Nothing is emulated
with a hidden extra sort key, so a page containing nulls comes back in the same
order wherever it runs.

### Operator vocabulary

| Operator | Meaning |
|----------|---------|
| `and`, `or`, `!` | Boolean combinators |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | Comparisons (`===`/`!==` are aliases). `{"==": [x, null]}` is a null test |
| `<`/`<=` (ternary) | Range: `{ "<=": [1, { "field": "x" }, 10] }` → BETWEEN |
| `in` | Membership (list haystack) or substring containment (string haystack) |
| `starts_with`, `ends_with` | Text anchors (rendered as `LIKE` / `$regex` / `prefix`+`wildcard`). Case sensitivity is **backend-defined** — see the parity table |
| `missing` | Field(s) have no meaningful value |
| `some`, `all`, `none` | Quantifiers over a declared relation |

Anything outside this vocabulary is rejected with a **located error** naming
the operator and position — never silently ignored. The same strictness
applies to the envelopes themselves: an unknown key in the query or write
envelope (a `"fileds"` typo, say) is rejected naming the key, because a
silently dropped key is a filter or projection silently not applying.

## Write envelope (`data_write`)

The envelope is nested under `write`, mirroring `data_query`'s `query`. The
handler's own keys — `connector`, `schema`, `params`, `database`, `output` —
stay at the top level, so the dialect and the handler never share a namespace
and there is a single JSON value that *is* the envelope.

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
    "connector": "orders_db", "output": "data.created",
    "write": {
      "op": "insert", "target": "users",
      "values": [ { "name": "Ada", "status": "active" }, { "name": "Grace", "status": "active" } ],
      "returning": ["id"] } } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "output": "data.updated",
    "params": { "id": { "var": "data.req.id" } },
    "write": {
      "op": "update", "target": "users",
      "set": { "status": "inactive" },
      "filter": { "==": [{ "field": "id" }, { "param": "id" }] } } } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "output": "data.upserted",
    "write": {
      "op": "upsert", "target": "users",
      "values": { "email": "ada@x.io", "name": "Ada" },
      "on_conflict": { "target": ["email"], "action": "update" } } } }
```

> **Upgrading from 0.3.x.** The pre-1.0 flat form — `op`/`target`/`values`/…
> alongside `connector` and `output` — is still accepted for one release, so
> existing workflows keep running. When a task carries both, `write` wins.

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
approximate silently.**

Everything not listed below returns the **same row set on all five backends**,
and that claim is executable: `tests/integration/data_parity_test.rs` runs one
fixture dataset through a table of envelopes and asserts an identical result —
or an identical capability error — on SQLite, PostgreSQL, MySQL, MongoDB and
Elasticsearch. This table is that table.

| Feature | Behavior |
|---------|----------|
| `returning` | Native on PostgreSQL/SQLite. On MySQL it is rejected (`FeatureUnsupportedByTarget`); single-row inserts report `last_insert_id` instead. On MongoDB inserts report generated `ids`. On Elasticsearch it is rejected; inserts report `ids` |
| `include` | SQL connectors only. On MongoDB and Elasticsearch it is rejected (`FeatureUnsupportedByTarget`) rather than returning parents with silently empty children — fetch related documents with a second query, or model them embedded/nested and filter with `some`. On SQL it requires a `sort` and is bounded per parent (see [Relations](#relations-and-includes)) |
| `some`/`all`/`none` over a `many_to_many` relation | SQL only (junction join). Rejected on MongoDB and Elasticsearch, whose filter languages cannot express the junction |
| `all` over an ES relation | Rejected (not set-equivalent on nested documents) |
| Deep ES pagination | `skip + limit` beyond `max_result_window` (10k) is rejected, not truncated |
| Bulk upsert on Mongo/ES | Rejected — single-row upserts only |
| The document key (`_id`) | **Explicit everywhere.** Neither MongoDB nor Elasticsearch maps a logical `id` to `_id` implicitly — declare it (`{"columns": {"id": {"name": "_id"}}}`). Without the rename, `id` is an ordinary field, which is what a collection carrying a genuine non-key `id` needs |
| ES upsert conflict target | Must resolve to the document `_id` (declare a schema rename); anything else is rejected |
| Text-match case sensitivity | **Backend-defined, and the one thing the dialect does not normalise.** PostgreSQL `LIKE` is case-sensitive; SQLite's `LIKE` folds ASCII (only); MySQL follows the column's collation (case-insensitive under the default `_ci` collations); MongoDB `$regex` is case-sensitive; Elasticsearch depends on the field mapping — `keyword` is exact, `text` matches against analyzer-folded tokens. It is a property of the stored data rather than of the query, and no query-time flag can make an analyzed ES field case-sensitive again, so it is stated here instead of being half-normalised. Use a `keyword` mapping / a binary collation when an exact match matters |

### Elasticsearch notes

- **`_id` lives outside `_source`.** With the rename declared, a physical `_id`
  column is lifted into the bulk action / URL path on insert and upsert;
  mutating `_id` in `set` is rejected.
- **Read-your-writes.** Every ES write requests a refresh (`wait_for`, or
  `true` on the by-query endpoints), so a `data_query` later in the same
  pipeline sees the write — parity with SQL/Mongo visibility, at a throughput
  cost.
- **`_bulk` is non-transactional.** Each action is applied independently, so
  any subset can land — see [Bulk writes](#bulk-writes). Version conflicts on
  `_update_by_query`/`_delete_by_query` surface as errors (`conflicts=abort`).

## Result shapes

Written to the task's `output` path:

| Backend | Shape |
|---------|-------|
| Query (all) | JSON array of rows/documents |
| SQL write | `{ "status": "ok", "rows_affected": n }`, plus `"returning": [..]` where supported and `"last_insert_id": n` on MySQL single-row inserts |
| MongoDB / ES write | `"status"` plus doc-store keys per op: `{ "inserted": n, "ids": [..] }`, `{ "matched": n, "modified": n }` (+ `"upserted_id"` when created), `{ "deleted": n }` |

Every write result carries a **`status`** — `"ok"` or `"partial"` — so one
check works across three backends whose failure models genuinely differ.

## Bulk writes

A bulk `insert` (an array of `values`) means three different things
underneath. All three now report through one shape, but the guarantees are not
the same and no envelope can make them so:

| Backend | Model | On failure |
|---|---|---|
| SQL | **Atomic** | One `INSERT … VALUES (…), (…)` inside an explicit transaction: every row or none. The call fails and nothing is written |
| MongoDB | **Prefix-applied** | `insert_many` is ordered, so the server stops at the first rejected document. Everything before it is committed; everything after is never attempted |
| Elasticsearch | **Arbitrary-applied** | `_bulk` attempts every action independently, so any subset can land |

When a call applies **some but not all** of its items, the result is
`"status": "partial"` and carries a per-item array, and the task reports audit
status **`207`** rather than `200` — visible in the trace, not fatal, so the
workflow can compensate:

```json
{
  "status": "partial",
  "inserted": 2,
  "failed": 1,
  "skipped": 2,
  "ids": ["a", "c"],
  "items": [
    { "index": 0, "status": "ok", "id": "a" },
    { "index": 1, "status": "ok", "id": "c" },
    { "index": 2, "status": "error", "error": { "code": 11000, "message": "duplicate key" } },
    { "index": 3, "status": "skipped" },
    { "index": 4, "status": "skipped" }
  ]
}
```

`index` is the position in the `values` array you sent. `skipped` means the
backend never attempted the item — only ordered MongoDB produces it. `items`
and the `failed`/`skipped` counters appear only when there is something to
report; a clean bulk is just `status`/`inserted`/`ids`.

**A partial write does not fail the task.** Erroring would abort the workflow
while leaving the applied prefix unnamed, which is the thing this reports —
so a workflow that writes in bulk to MongoDB or Elasticsearch should check
`status` and compensate. A bulk where *nothing* landed is still a hard error:
there is no partial state to describe.

## The schema registry

Both functions take an inline `schema` — **privileged configuration authored
alongside the workflow, never built from request input**. It is what bounds the
call: since 1.0 the dialect **rejects undeclared names by default**, so a task
with no `schema` reaches nothing.

Through 0.x the default was the opposite — *identity mode*, where every name
passed through to the physical one — which meant any workflow author reached
every table the connector's database user could see, read and write, unless
they remembered to opt in to `"unmapped": "reject"` on that task. Identity mode
is still available, but it now has to be asked for —
`"schema": { "unmapped": "identity" }`.

An undeclared name reports what to add, naming both routes:

```
entity 'orders' is not declared in the task's schema: add "schema":
{"entities": {"orders": {"columns": {"<column>": {}}}}} naming the columns this
task uses, or add "unmapped": "identity" to that schema to accept undeclared
names as physical ones (pre-1.0 behaviour)
```

A relation's `to` target does not itself need declaring *to resolve the
relation* — like a relation's join keys, it is structure the schema's author
wrote, not a caller-supplied name. Naming one of its **columns** is caller
input again, so `include: { "orders": {} }` works against an undeclared
`orders` while `include: { "orders": { "fields": ["id"] } }` — or any
`some`/`all`/`none` predicate over it — needs `orders` declared.

```json
"schema": {
  "entities": {
    "users": {
      "physical": "app_users",
      "columns": {
        "id":     { "name": "user_id", "type": "int" },
        "email":  { "type": "text" },
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
  (`id` → `user_id`, or `id` → `_id` on **both** document stores: neither
  Elasticsearch nor MongoDB maps a logical `id` onto the document key
  implicitly, so targeting it is always an explicit rename).
- **Types** — drive value coercion where a backend needs the hint.
- **Allowlist** — under `"unmapped": "reject"` (the default), only declared
  entities and columns are usable; `queryable: false` hides a column from
  reads, `writable: false` protects it from writes (generated/identity
  columns). A read that names no `fields` is **projected**, not a wildcard:
  it returns exactly the entity's queryable columns, so `queryable: false`
  means the same thing whether or not the caller listed `fields`. An entity
  that declares *no* columns (a relation-only or write-only declaration) has
  no column allowlist to apply and still reads every column; one that declares
  columns and marks them all non-queryable is refused rather than widened back
  to `SELECT *`.
- **Relations** — declare `has_one` / `has_many` / `many_to_many` (the latter
  via `through`) so `some`/`all`/`none` predicates and `include` work.

## Relations and includes

With relations declared, a filter can quantify over related records:

```json
{ "some": [{ "field": "orders" }, { ">": [{ "field": "total" }, 100] }] }
```

renders as a correlated `EXISTS` (SQL), `$elemMatch` over embedded documents
(Mongo), or a `nested`/`has_child` query (ES). `include` fetches the related records
themselves, hydrated with one child query per relation:

```json
"include": { "orders": { "fields": ["id", "total"], "sort": [{ "total": "desc" }], "limit": 10 } }
```

**`sort` is required.** The per-parent page is cut inside the database
(`ROW_NUMBER() OVER (PARTITION BY <fk> ORDER BY <sort>)`), so without an order
key "the first 10 orders" has no defined answer — it would be whichever ten rows
the plan happened to emit, and a different ten on the next run. `limit` follows
the envelope's own page policy: absent means `query.default_limit` **per
parent**, and a value above `query.max_limit` is rejected rather than clamped.
Hydration is therefore bounded by `parents × limit` rows, not by the whole child
table.

`sort` may name a column that `fields` does not: it is projected internally so
the database can order by it, then removed again, so the nested objects carry
exactly the `fields` that were asked for (and every column when `fields` is
absent). The join key is handled the same way.

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

### Schema guards

`operations` answers *which verbs*; the `dialect` block answers *which tables
the portable dialect may name*. A `read`-only connector is otherwise unbounded
through `data_query` — nothing stops a workflow from selecting the whole
database.

**These guards bound `data_query`/`data_write`, not the connector.** The raw
escape hatches — `db_read`, `db_write`, `mongo_read` — run on the same
connector, carry no entity name a guard could match, and are bounded only by
`operations`. So:

- **Writes** are fully bounded by `allowed_entities` once `"raw_write": false`
  leaves `data_write` as the only write path.
- **Reads are not**, because `read` gates `data_query`, `db_read` and
  `mongo_read` together: a connector that permits the dialect permits raw SQL
  and raw `find` too. Bound those at the database credential — a role that can
  only see the allowlisted tables — or keep raw reads on a separate connector.
- On MongoDB the task's `database` field is not checked against the guard
  either, so an allowlisted collection name can be read from any database the
  credential can see. Scope the credential, not just the list.

```json
"config": {
  "type": "db",
  "connection_string": "postgres://…",
  "dialect": {
    "require_schema": true,
    "allowed_entities": ["users", "orders", "order_items"]
  }
}
```

| Field | Default | Effect |
|---|---|---|
| `require_schema` | `false` | Refuse any dialect call that did not declare a real schema — no `entities`, or an explicit `"unmapped": "identity"`. Closes the per-task opt-out, so one forgotten `schema` key cannot reopen the connector |
| `allowed_entities` | `[]` (unrestricted) | Physical table/collection/index names `data_query`/`data_write` may name through this connector |

`allowed_entities` matches the name **after** schema renames apply, because the
allowlist is the connector owner's and the schema is authored per task — a
rename (`"orders"` → physical `secrets`) must not be able to step around it. It
covers every table a call reaches: the envelope's `source`/`target`, relation
targets, and many-to-many junction tables.

Both default to off. The 1.0 flip of `unmapped` to `reject` is what makes the
safe mode the default one; these exist for what that flip cannot cover, since a
task can still write `"unmapped": "identity"` itself and only the connector's
owner can say that is not allowed here.

## Configuration

```toml
[query]                    # Page-size bounds for data_query
# default_limit = 100      # Page size when a query omits `limit`
# max_limit = 1000         # Hard cap; a query asking for more is rejected
# max_skip = 10000         # Hard cap on the `skip` offset (over → rejected)

[write]                    # Safety bounds for data_write
# max_rows = 1000          # Cap on rows per bulk insert/upsert (over → rejected)
# allow_unfiltered = false # Permit unfiltered update/delete (still needs per-call "all": true)
```

Both sections are overridable via environment variables
(`ORION_QUERY__MAX_LIMIT`, `ORION_WRITE__MAX_ROWS`, …).
