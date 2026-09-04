<!-- description: Express a query or mutation once and run it unchanged against PostgreSQL, MySQL, SQLite, MongoDB or Elasticsearch — Orion's portable, injection-safe dialect. -->
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

The raw functions (`db_read`/`db_write` for SQL; `mongo_read`/`mongo_write`/
`mongo_aggregate` for MongoDB) remain the **escape hatch** for anything
outside the portable vocabulary — hand-written SQL, CTEs, multi-table
statements, aggregation pipelines, nested document writes.

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

A complete call does two more things. It binds request values through `params`,
and it declares the entities it touches in `schema`. A call with no schema is
refused — see [The schema registry](#the-schema-registry):

```json
{
  "name": "data_query",
  "input": {
    "connector": "orders_db",
    "query": {
      "source": "users",
      "filter": { "==": [{ "field": "id" }, { "param": "id" }] },
      "sort": [{ "age": "asc" }],
      "limit": 50
    },
    "params": { "id": { "var": "data.req.id" } },
    "schema": {
      "entities": {
        "users": {
          "columns": {
            "id":   { "type": "int" },
            "name": { "type": "text" },
            "age":  { "type": "int" }
          }
        }
      }
    },
    "output": "data.users"
  }
}
```

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

### Extended-JSON values

Two extended-JSON wrappers are accepted wherever a scalar value is — filter
comparisons, `in` haystacks, and `data_write`'s `values`/`set`:

| Wrapper | Meaning | Accepted payloads |
|---------|---------|-------------------|
| `{ "$oid": "<24 hex>" }` | A BSON ObjectId | hex string |
| `{ "$date": … }` | A BSON typed date | RFC 3339 string, epoch milliseconds, or canonical `{"$numberLong": "<millis>"}` |

The payload may itself be a `{ "param": "p" }` node, so a per-request id
composes as `{ "$oid": { "param": "id" } }`; and a param whose *value* is a
wrapper object (message data echoing a `mongo_read` result, which serializes
ObjectIds and dates in exactly these spellings) coerces the same way.

The payload is validated during lowering — a malformed `$oid` is a located
envelope error on every backend. On **MongoDB** the wrappers render as native
BSON values, so filtering on a real `_id` or a date range matches typed data
instead of silently missing. On **SQL and Elasticsearch** they raise the
standard capability error (`FeatureUnsupportedByTarget`) — an ISO date on
those backends is already expressible as a plain string; the wrapper exists
for BSON's typed values, and rendering it as anything else would compare
differently than on Mongo. Any other object or array value remains
not-representable, exactly as before.

## Query envelope (`data_query`)

| Field | Type | Description |
|-------|------|-------------|
| `source` | string | Logical entity → table / collection / index (schema-resolved) |
| `filter` | JSONLogic | Condition from the operator vocabulary below; omit for "match all" |
| `fields` | array | Projection; omit for all columns/fields |
| `sort` | array | `[{ "age": "asc" }, { "name": "desc" }]`. A null (or missing) value sorts as the smallest value — first on `asc`, last on `desc` — on every backend |
| `limit` / `skip` | number | Pagination. A missing `limit` gets `query.default_limit`; a `limit` above `query.max_limit` or a `skip` above `query.max_skip` is **rejected, never clamped** |
| `include` | object | Relation name → `{ "fields": [..], "sort": [..], "limit": n }`; nested related records, hydrated per relation (see [Relations](#relations-and-includes)) |

Null-first ordering is the native order of SQLite, MySQL, and MongoDB;
PostgreSQL and Elasticsearch receive an explicit `NULLS FIRST` / `missing`
clause to match. No hidden sort key is added, so a page containing nulls comes
back in the same order on every backend.

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

An operator outside this vocabulary is rejected with a **located error**
naming the operator and its position — never silently ignored. An unknown key
in a query or write envelope (a `"fileds"` typo, say) is rejected naming the
key: a silently dropped key would be a filter or projection silently not
applying.

### Range, null and quantifier semantics

These rules are **normative** — every renderer implements them, and the
cross-backend parity suite pins them:

- **Chained ranges keep each bound's strictness.** An inclusive chain
  `{ "<=": [1, {"field": "x"}, 10] }` renders as an inclusive `BETWEEN` (or
  its backend equivalent). A strict chain (`<`) renders as per-bound
  comparisons — never widened into `BETWEEN` — and a mixed chain keeps the
  strict side strict. The descending spellings
  `{ ">": [10, {"field": "x"}, 1] }` / `">="` denote the same ranges with the
  operands reversed.
- **Empty combinators fold identically everywhere.** `{"and": []}` folds to
  *true*, `{"or": []}` to *false*, and an empty `in` list to *false* — at
  lowering time, before any backend sees the filter.
- **`null` comparisons are existence tests.** `{"==": [{"field": "x"}, null]}`
  means "x has no value" (SQL `IS NULL`, Mongo/ES missing-or-null), and `!=`
  null is its negation — never a literal comparison that SQL three-valued
  logic would swallow.
- **`all` counts an unevaluable element as a violation.** `all` over a
  relation means the relation is non-empty **and** no element violates the
  predicate; an element whose predicate evaluates to SQL `NULL` counts as
  violating. SQL renders
  `EXISTS(rel) AND NOT EXISTS(rel WHERE NOT cond OR cond IS NULL)`; MongoDB
  renders the equivalent; Elasticsearch rejects `all` outright (see the
  parity table) rather than approximating it on nested documents.

## Write envelope (`data_write`)

The envelope is nested under `write`, mirroring `data_query`'s `query`; the
handler's own keys — `connector`, `schema`, `params`, `database`, `output` —
stay at the top level. `write` is required: a task without it is refused at
create, update, import, `POST /admin/workflows/validate`, and
`orion-server lint`.

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
and that claim is executable:
`crates/orion-server/tests/integration/data_parity_test.rs` runs one
fixture dataset through a table of envelopes and asserts an identical result —
or an identical capability error — on SQLite, PostgreSQL, MySQL, MongoDB and
Elasticsearch. The table below is the complete list of divergences.

| Feature | Behaviour |
|---------|----------|
| `returning` | Native on PostgreSQL/SQLite. Rejected (`FeatureUnsupportedByTarget`) on MySQL, MongoDB and Elasticsearch — asking for it there is an error, never a write that quietly answers without it. What each does report instead: MySQL single-row inserts carry `last_insert_id`; MongoDB and Elasticsearch inserts carry the generated `ids` |
| `include` | SQL connectors only. Rejected on MongoDB and Elasticsearch (`FeatureUnsupportedByTarget`) rather than returning parents with silently empty children — fetch related documents with a second query, or model them embedded/nested and filter with `some`. On SQL it requires a `sort` and is bounded per parent (see [Relations](#relations-and-includes)) |
| `some`/`all`/`none` over a `many_to_many` relation | SQL only (junction join). Rejected on MongoDB and Elasticsearch, whose filter languages cannot express the junction |
| `all` over an ES relation | Rejected (not set-equivalent on nested documents) |
| Deep ES pagination | `skip + limit` beyond `max_result_window` (10k) is rejected, not truncated |
| Bulk upsert on Mongo/ES | Rejected — single-row upserts only |
| `on_conflict.target` on MySQL | **Advisory.** MySQL's `ON DUPLICATE KEY UPDATE` cannot name a conflict target: the upsert fires on *any* unique index on the table, including the primary key and unique indexes the envelope never mentions. PostgreSQL and SQLite key on exactly the declared columns. The declared target is still required, and still resolved through the schema, but on MySQL it selects nothing — so an upsert on a table with more than one unique index can update a row the target did not identify. Model the table with a single unique key, or keep the upsert on PostgreSQL/SQLite, when that difference matters |
| The document key (`_id`) | **Explicit everywhere.** Neither MongoDB nor Elasticsearch maps a logical `id` to `_id` implicitly — declare the rename (`{"columns": {"id": {"name": "_id"}}}`). Without it, `id` is an ordinary document field |
| ES upsert conflict target | Must resolve to the document `_id` (declare a schema rename); anything else is rejected |
| Text-match case sensitivity | **Backend-defined; the dialect does not normalize it.** PostgreSQL `LIKE` is case-sensitive; SQLite's `LIKE` folds ASCII only; MySQL follows the column's collation (case-insensitive under the default `_ci` collations); MongoDB `$regex` is case-sensitive; Elasticsearch follows the field mapping (`keyword` exact, `text` analyzer-folded). Use a `keyword` mapping or a binary collation when an exact match matters |
| [Extended-JSON values](#extended-json-values) (`$oid`, `$date`) | Native BSON on MongoDB. Rejected on SQL and Elasticsearch (`FeatureUnsupportedByTarget`) — a malformed payload is a located envelope error on every backend |

<details><summary>Why case sensitivity is not normalized</summary>

Case behaviour is a property of the stored data rather than of the query — no
query-time flag can make an analyzed Elasticsearch field case-sensitive again.
The dialect states the divergence instead of half-normalizing it.

</details>

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
check works across all backends.

## Bulk writes

A bulk `insert` — an array of `values` — reports through one shape on every
backend. The underlying guarantee differs, and no envelope can make the three
models the same:

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

**A partial write does not fail the task.** Failing would abort the workflow
without naming the applied prefix — the thing this result reports. A workflow
that writes in bulk to MongoDB or Elasticsearch should check `status` and
compensate. A bulk where *nothing* landed is still a hard error: there is no
partial state to describe.

## The schema registry

Both functions take an inline `schema` — **privileged configuration authored
alongside the workflow, never built from request input**. It is what bounds the
call: the dialect **rejects undeclared names by default**, so a task with no
`schema` reaches nothing. Identity mode — every logical name passing through
as the physical one — must be requested explicitly:
`"schema": { "unmapped": "identity" }`.

An undeclared name reports what to add, naming both routes:

```
entity 'orders' is not declared in the task's schema: add "schema":
{"entities": {"orders": {"columns": {"<column>": {}}}}} naming the columns this
task uses, or add "unmapped": "identity" to that schema to accept undeclared
names as physical ones (pre-1.0 behaviour)
```

A relation's `to` target does not itself need declaring for the relation to
resolve — like its join keys, it is structure the schema's author wrote, not a
caller-supplied name. Naming one of its **columns** is caller input again:
`include: { "orders": {} }` works against an undeclared `orders`, while
`include: { "orders": { "fields": ["id"] } }` — or any `some`/`all`/`none`
predicate over it — needs `orders` declared.

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
  (`id` → `user_id`, or `id` → `_id` on both document stores — see the
  parity table).
- **Types** — declared hints (`int`, `text`, …), validated at parse time but
  not consumed: values keep their natural JSON types end to end, and no
  backend coerces on the hint. The key is reserved for value coercion in a
  later version; an unknown type name is a hard error.
- **Allowlist** — under `"unmapped": "reject"` (the default), only declared
  entities and columns are usable. `queryable: false` hides a column from
  reads; `writable: false` protects it from writes (generated/identity
  columns). A read that names no `fields` returns exactly the entity's
  queryable columns — a projection, not a wildcard. An entity that declares
  *no* columns has no column allowlist and still reads every column; one whose
  declared columns are all non-queryable is refused rather than widened back
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
key "the first 10 orders" has no defined answer. `limit` follows the envelope's
own page policy: absent means `query.default_limit` **per parent**, and a value
above `query.max_limit` is rejected rather than clamped. Hydration is therefore
bounded by `parents × limit` rows, not by the whole child table.

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
| `read` | `data_query`, `db_read`, `mongo_read`, `mongo_aggregate` |
| `insert` / `update` / `delete` / `upsert` | the matching `data_write` op, and the matching `mongo_write` op |
| `raw_write` | the raw-SQL `db_write` escape hatch |

A gated call fails with a validation error naming the operation and connector
(`operation 'delete' is disabled on connector 'orders-db-readonly'`). Because
raw SQL cannot be classified per-statement, `db_write` has its own `raw_write`
gate — to make a connector fully delete-proof, disable both `delete` and
`raw_write`.

The gates above are the `db` / `es` set — the set the portable dialect runs
through. Other connector types carry gates for their own operations
(`read`/`write` on `cache`, `publish` on `kafka`, a method allowlist on
`http`), documented per type in [Connectors](./connectors.md).

### Schema guards

`operations` answers *which verbs*; the `dialect` block answers *which tables
the portable dialect may name*. A `read`-only connector is otherwise unbounded
through `data_query` — nothing stops a workflow from selecting the whole
database.

**These guards bound `data_query`/`data_write`, not the connector.** The raw
escape hatches — `db_read`, `db_write`, and the `mongo_read`/`mongo_write`/
`mongo_aggregate` trio — run on the same connector, carry no entity name a
guard could match, and are bounded only by `operations`. So:

- **SQL writes** are fully bounded by `allowed_entities` once
  `"raw_write": false` leaves `data_write` as the only write path; on MongoDB
  the per-op gates also cover `mongo_write`, but its collection names are not
  matched against the guard.
- **Reads are not**, because `read` gates `data_query`, `db_read`,
  `mongo_read` and `mongo_aggregate` together: a connector that permits the
  dialect permits raw SQL and raw `find` too. Bound those at the database
  credential — a role that can only see the allowlisted tables — or keep raw
  reads on a separate connector.
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

Both default to off. They bound what the schema default cannot: a task can
write `"schema": { "unmapped": "identity" }` itself, and only the connector's
owner can refuse that through `require_schema`.

## Configuration

The `[query]` and `[write]` sections — `default_limit`, `max_limit`,
`max_skip`, `max_rows`, `allow_unfiltered` — are documented in the
[Configuration Reference](./configuration.md).

## Related

- [Function Reference](./functions.md#data_query) — the `data_query` and
  `data_write` task fields that carry these envelopes.
- [Connectors](./connectors.md) — `db`/`es` connector configuration, including
  the operation gates each type carries.
- [Configuration Reference](./configuration.md) — the `[query]`/`[write]`
  server bounds and their environment overrides.
