# Proposal: a portable write dialect — `data_write` for SQL, MongoDB, and Elasticsearch

**Status:** Implemented (SQL + MongoDB + Elasticsearch live).
The handler is `data_write` in `src/engine/functions/data_write.rs`; the envelope and
resolution live in `src/query/write.rs`, the renderers in
`src/query/backend/{sql,mongo,es}.rs`.
**Layer:** An **Orion** feature, the write counterpart to the query dialect
(`proposals/query-dialect.md`). datalogic-rs is not involved; dataflow-rs gains
nothing new. It lives entirely in Orion, alongside the connectors, pools, drivers,
and the query dialect it extends.
**Goal:** Let a workflow express one database-agnostic **mutation** — insert,
update, delete, or upsert — once, in the same familiar JSONLogic-shaped syntax the
query dialect already uses, and run it unchanged against a SQL database (via
sea-query), MongoDB, or Elasticsearch. Switching backend is a connector change, not
a rewrite.

Where `data_query` reads, `data_write` writes. The two share an IR, a lowering
pass, a schema registry, and the SQL/Mongo condition renderers; the net-new surface
is the write envelope, the `INSERT`/`UPDATE`/`DELETE`/upsert renderers, and the
`data_write` handler. `db_read`/`db_write` remain the raw-SQL escape hatch for
anything outside the portable vocabulary.

---

## 1. Why Orion, why a write dialect, and where the boundary is

The query dialect proved the shape: Orion already owns everything a portable data
operation needs — connectors (`src/connector/config.rs`), pooled clients
(`SqlPoolCache` over `sqlx::AnyPool`, `MongoPoolCache`), circuit breakers, secrets,
and the **sea-query 0.32 + sea-query-binder 0.7** dependencies (the `sqlx-any`
feature already enabled). It went further and built, in `src/query/`, a
backend-neutral condition IR (`Cond`), a lowering pass that walks a curated
JSONLogic subset into it, an `EntityRegistry` for renames/types/allowlist/relations,
and renderers for SQL, MongoDB, and Elasticsearch.

A write dialect is a **small layer over that existing machinery**, not a second
engine. The one genuinely new thing is turning a set of column→value assignments
into a native `INSERT`/`UPDATE`/`DELETE`/upsert. The `WHERE` clause of an update or
delete is *literally the query dialect's filter* — same operators, same lowering,
same `Cond`, same SQL/Mongo renderers, including relation predicates
(`some`/`all`/`none` → `EXISTS`). This reuse is the core argument of the proposal
and the reason the write path is cheap to build.

### The honest boundary

As the query dialect states its fidelity limits (query-dialect.md §3.3), the write
dialect states its scope limits up front. **The portable vocabulary covers the
common single-table CRUD shape**: insert one or many rows, update/delete rows
matching a filter, upsert on a declared conflict key. Anything outside that —
multi-table writes, CTEs, `UPDATE ... FROM`, SQL functions in `SET`
(`col = col + 1`), triggers-as-contract, batched heterogeneous statements in one
atomic unit — stays in **`db_write`**, the raw-SQL escape hatch, which is unchanged.
`data_write` never tries to be a general SQL generator; it is a portable form for
the 90% case, with `db_write` as the always-available fall-through.

### A note on `db_write`

`db_write` (`src/engine/functions/db_write.rs`) runs a hand-written SQL string with
positional `?` params through `sqlx::query(..).execute()` and returns
`{ "rows_affected": n }`. It is dialect-specific (the author writes Postgres- or
MySQL-flavoured SQL), has no schema/allowlist, and has no MongoDB analogue.
`data_write` is to `db_write` what `data_query` is to `db_read`: portable,
schema-aware, injection-safe by construction, and backend-neutral. Both keep their
places.

---

## 2. Where this lands in Orion

New files, plus the usual registration edits. The layout mirrors the query dialect
so the two read as a pair.

| File | Responsibility |
|------|----------------|
| `src/query/write.rs` | write envelope parse: `WriteSpec` (`op` / `target` / `values` / `set` / `filter` / `on_conflict` / `returning` / `all`) |
| `src/query/backend/sql.rs` *(edit)* | add `render_insert` / `render_update` / `render_delete` / upsert over `sea_query::{Insert,Update,Delete}Statement`; `build_write_for`; expose the existing `render_expr` as `pub(crate)` |
| `src/query/backend/mongo.rs` *(edit)* | add `render_insert` / `render_update` / `render_delete` (reusing the Mongo condition renderer for the filter) |
| `src/query/backend/es.rs` *(edit)* | add `EsWrite` + `render_write` (`_bulk` / `_update_by_query` / `_delete_by_query` / `_update` / `_doc?op_type=create`), reusing the query renderer for filters |
| `src/query/mod.rs` *(edit)* | re-export the write API (`resolve_write`, `ResolvedWrite`, `WriteError`, `WriteOp`) alongside the query API |
| `src/engine/functions/data_write.rs` | the `data_write` `AsyncFunctionHandler` |
| `src/config/write.rs` | `WriteConfig { max_rows, allow_unfiltered }` |

Wiring follows the exact pattern `data_query` used:

1. `src/engine/functions/mod.rs`: add `pub mod data_write;`
2. `src/engine/mod.rs`: add `"data_write"` to `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS`
3. `src/engine/mod.rs` `build_custom_functions()`: register
   `DataWriteHandler { pool_cache, mongo_pool_cache, http_client, registry, max_rows, allow_unfiltered }`
4. `src/engine/functions/schema.rs`: add `DATA_WRITE_FIELDS` and a `REGISTRY` entry
5. `src/config/mod.rs`: add a `write: WriteConfig` section and call `write.validate()`

The handler reuses the existing connector/execution helpers from
`connector_helpers.rs` — `resolve_connector`, `require_str_field`,
`apply_output`, `extract_output_path`, `timed_query`, `profile_handler`,
`to_exec_error`, `rows_to_json` (for `RETURNING`) — plus `SqlPoolCache::get_pool` /
`MongoPoolCache::get_client` and `detect_backend` for the dialect. The
`resolve_params()` helper lives in `connector_helpers.rs`, shared by both
handlers.

---

## 3. The common write model

### 3.1 The envelope

```json
{
  "connector": "orders_db",
  "op": "update",
  "target": "users",

  "values": { "id": 7, "name": "Ada", "status": "active" },
  "set":    { "status": { "param": "s" } },
  "filter": { "==": [ { "field": "id" }, { "param": "id" } ] },

  "on_conflict": { "target": ["email"], "action": "update" },
  "returning":   ["id", "status"],
  "all":         false,

  "params": { "id": { "var": "data.req.id" }, "s": { "var": "data.req.status" } },
  "schema": { "entities": { "users": { "columns": { "id": { "type": "int" } } } } },
  "output": "data.write_result"
}
```

| Field | Meaning | Used by |
|-------|---------|---------|
| `connector` | connector name; its type + connection string select the backend and dialect | all |
| `op` | `insert` \| `update` \| `delete` \| `upsert` | all |
| `target` | logical entity → table / collection / index (schema-resolved) | all |
| `values` | a row object, or an array of row objects (bulk) | insert, upsert |
| `set` | column → value/param assignments | update, upsert |
| `filter` | JSONLogic condition selecting rows — **the query dialect's filter** | update, delete |
| `on_conflict` | conflict `target` columns + `action` (`update` \| `nothing`) | upsert |
| `returning` | columns to return from mutated rows (capability-gated, §6) | insert, update, delete, upsert |
| `all` | explicit acknowledgement for an intentionally unfiltered update/delete (§6) | update, delete |
| `params` | named values folded into `{"param": ..}` nodes in `values`/`set`/`filter` | all |
| `schema` | optional inline `EntityRegistry` — renames, types, allowlist, writable flag | all |
| `output` | dotted message path for the result | all |

### 3.2 The token model (identical to `data_query`)

The three tokens never collide, exactly as in the query dialect (query-dialect.md
§3.4):

- **`field`** — a column, valid only inside `filter` (and, for symmetry, as a
  key name in `set`/`values`, which are plain objects of column→value).
- **`param`** — a named value reference, valid inside `filter`, `set`, and
  `values`; resolved to a literal *before* rendering from the `params` map.
- **`var`** — an ordinary datalogic context lookup, valid only inside the `params`
  map, resolving against the message context (`{data, metadata, temp_data}`).

`params` are folded into literals before rendering, then bound (SQL) or placed as
document values (Mongo/ES). No user value ever reaches the backend as string
interpolation — the same injection-safety guarantee the query dialect makes.

### 3.3 Values and coercion

A `values`/`set` value is a JSON scalar (or `{"param": ..}` resolving to one). It is
coerced to the IR `Value` (`src/query/ir.rs`) and then to an `AnyPool`-safe
`sea_query::Value`. The query dialect already pins the safe set: **Null / Bool /
Int(i64) / Float(f64) / Str** — never `Decimal`/`Json`/`Uuid`, which panic under the
`sqlx-any` binder (see the `ir::Value` doc comment). The write path inherits this
exactly. A new `json_to_sea_value()` is needed because the existing
`json_key_to_sea()` (used for `IN` list keys) deliberately **skips null**, whereas
an `INSERT`/`SET` must bind a JSON `null` as SQL `NULL`.

With a schema, a column's declared `FieldType` drives coercion (e.g. a date string,
an int-vs-string binding). Without one (identity mode), coercion is the best-effort
per-backend default — the same honesty boundary as reads.

---

## 4. The dialect: backend mapping

One `WriteSpec` renders three ways. SQL is expressed through sea-query (text +
parameter style are the builder's job). MongoDB is a driver call with BSON
documents. Elasticsearch is a query-DSL body over HTTP.

### 4.1 The operation table

| `op` | SQL (sea-query) | MongoDB | Elasticsearch |
|------|-----------------|---------|---------------|
| `insert` (single) | `INSERT INTO t (cols) VALUES (…)` | `insertOne(doc)` | `POST t/_bulk` (one index action; a physical `_id` column becomes the action id) |
| `insert` (bulk) | `INSERT INTO t (cols) VALUES (…), (…), …` | `insertMany([docs])` | `POST t/_bulk` (index actions) |
| `update` | `UPDATE t SET … WHERE <render_expr(filter)>` | `updateMany(filter, { $set })` | `POST t/_update_by_query { query, script }` (painless; field names and values as script params) |
| `delete` | `DELETE FROM t WHERE <render_expr(filter)>` | `deleteMany(filter)` | `POST t/_delete_by_query { query }` |
| `upsert` | `INSERT … ON CONFLICT (target) DO UPDATE SET … / DO NOTHING` (PG/SQLite); `INSERT … ON DUPLICATE KEY UPDATE …` (MySQL) | `updateOne(filter, { $set }, { upsert: true })` | `POST t/_update/{id} { doc, doc_as_upsert / upsert }` (action `update`); `PUT t/_doc/{id}?op_type=create` with 409-as-no-op (action `nothing`) |

The **`filter` of `update`/`delete` is rendered by the exact same code path as a
`data_query` `WHERE`**: `lower::lower_with(filter, params, reg, target)` → `Cond` →
`backend::sql::render_expr(cond, table)` (SQL) or the Mongo condition renderer. This
includes relation predicates: `DELETE FROM users WHERE EXISTS(SELECT 1 FROM orders
…)` falls out for free from `some`/`all`/`none`. No new WHERE-clause semantics are
written.

### 4.2 SQL rendering with sea-query

The three statement builders sit next to the existing `render()` in
`backend/sql.rs` and reuse its `render_expr`, `col_expr`, and `to_sea_value`:

```rust
// INSERT (single or bulk)
let mut q = Query::insert();
q.into_table(Alias::new(table)).columns(cols.iter().map(Alias::new));
for row in rows { q.values_panic(row.iter().map(json_to_sea_value)); }
if !returning.is_empty() { q.returning(Query::returning().columns(returning_cols)); }

// UPDATE
let mut q = Query::update();
q.table(Alias::new(table));
for (col, val) in set { q.value(Alias::new(col), json_to_sea_value(val)); }
q.cond_where(render_expr(&cond, table)?);   // <-- the query dialect's filter

// DELETE
let mut q = Query::delete();
q.from_table(Alias::new(table)).cond_where(render_expr(&cond, table)?);
```

`build_write_for(dialect, &stmt)` produces `(String, SqlxValues)` for each statement
type — sea-query's `SqlxBinder` is implemented for `InsertStatement`,
`UpdateStatement`, and `DeleteStatement`, so this is the same `build_sqlx(builder)`
match already used by `build_for` for `SelectStatement`, generalised. Dialect comes
from the connection-string scheme via `detect_backend` (query-dialect.md §8), never
`get_backend()`.

Upsert uses sea-query's `on_conflict` for Postgres/SQLite and the MySQL builder's
`ON DUPLICATE KEY` form; the divergence is called out in §6.

### 4.3 MongoDB rendering

Values become BSON via the `bson::to_document` path `mongo_read` already uses.
`insert` → `insert_one`/`insert_many`; `update` → `update_many(filter_doc, {"$set":
set_doc})`; `delete` → `delete_many(filter_doc)`; `upsert` →
`update_one(filter_doc, {"$set": set_doc}, UpdateOptions::builder().upsert(true))`.
The `filter_doc` is produced by the same Mongo condition renderer `data_query` uses.
`id`→`_id` renaming and other logical→physical names come from the schema, applied
during lowering, so the renderer sees physical names.

### 4.4 Elasticsearch rendering

`render_write` in `backend/es.rs` produces an `EsWrite` (`BulkInsert` /
`UpdateByQuery` / `DeleteByQuery` / `UpdateDoc` / `CreateDoc`); the handler maps
each variant to its endpoint and normalises the response (§7). The update/delete
`filter` is rendered by the same `query_json` the search path uses — including
`match_all` for an acknowledged unfiltered mutation.

The `_id` contract: ES documents are keyed on the metadata `_id`, which lives
*outside* `_source`. Unlike Mongo there is **no implicit `id` → `_id` mapping** —
a genuine `id` field inside `_source` is legal — so the rename is an explicit
schema decision (`{"columns": {"id": {"name": "_id"}}}`). A physical `_id` column
is lifted out of the source into the bulk action / URL path; upsert requires the
conflict target to resolve to `_id` (else a located capability error); a `set`
touching `_id` is rejected (`_id` is immutable).

Update scripts are injection-safe by construction: both field names and values
travel as painless `params` (`ctx._source[params.f0] = params.v0`), never spliced
into the script source.

---

## 5. The `data_write` function

A new `AsyncFunctionHandler`, registered exactly like `data_query`, with the same
struct fields plus the write config knobs:

```rust
pub struct DataWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    pub max_rows: u64,
    pub allow_unfiltered: bool,
}
```

Execution (mirrors `DataQueryHandler::execute`):

1. Resolve `params` against the message context (`resolve_params`) — the only point
   the message touches the mutation; it produces literals, not SQL.
2. Parse the envelope into a `WriteSpec` (`src/query/write.rs`), validating `op` and
   the fields required for it (`values` for insert/upsert, `set` for update/upsert,
   `filter` for update/delete unless `all`).
3. Parse the optional inline `schema` into an `EntityRegistry` (privileged config).
4. For update/delete, lower `filter` to `Cond` via `lower_with(.., target)`.
5. Dispatch on the connector type (`ConnectorConfig::Es` / `Db`-if-mongo / `Db`),
   render, and execute on the cached pool.
6. `apply_output(ctx, extract_output_path(input), result)`.

Example task JSON, one per op:

```json
{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "insert", "target": "users",
    "values": [ { "name": "Ada", "status": "active" }, { "name": "Grace", "status": "active" } ],
    "returning": ["id"], "output": "data.created" } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "update", "target": "users",
    "set": { "status": { "param": "s" } },
    "filter": { "==": [ { "field": "id" }, { "param": "id" } ] },
    "params": { "id": { "var": "data.req.id" }, "s": "inactive" },
    "output": "data.updated" } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "delete", "target": "sessions",
    "filter": { "<": [ { "field": "expires_at" }, { "param": "now" } ] },
    "params": { "now": { "var": "metadata.now" } }, "output": "data.deleted" } }

{ "name": "data_write", "input": {
    "connector": "orders_db", "op": "upsert", "target": "users",
    "values": { "email": "ada@x.io", "name": "Ada" },
    "on_conflict": { "target": ["email"], "action": "update" },
    "output": "data.upserted" } }
```

---

## 6. Divergence analysis and parity rules

Same governing rule as the query dialect: **parity-or-error**. Match the reference
semantics where a backend can; raise a precise, located error where it cannot; never
approximate silently.

### 6.1 The unfiltered-mutation guard (the headline safety rule)

An `update` or `delete` with **no `filter`** rewrites or removes *every* row. This is
almost never intended and is the single most dangerous shape a write dialect can
emit. Mirroring the query dialect's "never silently clamp a limit" stance, an
unfiltered mutation is **rejected** with a located error
(`WriteError::UnfilteredMutation { op }`) unless the envelope carries an explicit
`"all": true` acknowledgement **and** app config permits it
(`write.allow_unfiltered`, default `false`). The author must opt in twice — once per
call and once globally — to truncate a table. Golden and integration tests pin the
rejection.

### 6.2 Bulk insert cap

A bulk `insert` is bounded by `write.max_rows` (parallel to `query.max_limit`). A
`values` array longer than the cap is **rejected** with
`WriteError::TooManyRows { requested, max }`, never silently truncated — a truncated
insert is a silent data-loss bug.

### 6.3 RETURNING support matrix

`RETURNING` is not universal:

- **Postgres / SQLite**: native `RETURNING` — `returning` is honoured directly.
- **MySQL**: no `RETURNING`. For a single-row insert, return `last_insert_id` +
  `rows_affected`; for multi-row or update/delete, `returning` requested against
  MySQL raises `FeatureUnsupportedByTarget { feature: "returning", target: "mysql" }`
  rather than pretending. (A later phase may emulate it with a follow-up `SELECT`
  inside a transaction.)
- **MongoDB**: returns inserted/upserted `_id`s and match/modify counts (§7); a
  full `returning` projection of updated docs is a later addition.
- **Elasticsearch**: no `RETURNING`; a non-empty `returning` raises
  `FeatureUnsupportedByTarget { feature: "returning", target: "elasticsearch" }`.
  Inserts still report the document `ids` (§7).

This matrix is documented, not hidden — the parity-or-error contract.

### 6.4 Upsert conflict target and portability

Upsert needs a conflict target. `on_conflict.target` names the unique columns; the
schema should declare them unique. Postgres/SQLite take the column list directly
(`ON CONFLICT (email)`); MySQL infers it from the unique index (`ON DUPLICATE KEY
UPDATE`) and ignores an explicit target list — a divergence noted in the doc. Mongo
maps the conflict target into the `updateOne` filter. `action: "nothing"` maps to
`DO NOTHING` (PG/SQLite), a no-op update set (MySQL), and — for Mongo — `upsert:
true` with an empty `$set` on match. Where a backend cannot express the requested
action, `FeatureUnsupportedByTarget`.

### 6.5 Null vs. absent on update

SQL `SET col = NULL` always writes NULL. Mongo `{ "$set": { col: null } }` writes a
BSON null (distinct from `$unset`, which removes the field). The dialect defines a
`set` value of JSON `null` as **"set to null"** on both (SQL `NULL`, Mongo BSON
null); field removal (`$unset`) is out of the portable vocabulary in v1.

### 6.6 Elasticsearch write consistency

ES writes carry semantics SQL/Mongo do not; the live path handles each honestly
rather than pretending transactional parity:

- **Visibility.** ES writes are eventually visible by default, which would break
  a `data_query` read-back later in the same pipeline. Every write therefore
  requests a refresh — `refresh=wait_for` on the doc-level endpoints, and
  `refresh=true` on `_update_by_query`/`_delete_by_query` (which don't support
  `wait_for`) — giving read-your-writes parity with SQL/Mongo at a throughput
  cost. A per-envelope/config knob to relax this is future work.
- **`_bulk` partial failure.** `_bulk` is ordered but non-transactional. Any item
  error fails the whole call (parity with atomic SQL / ordered Mongo), though
  earlier items may already be applied — a documented divergence, not a silent one.
- **Version conflicts.** The by-query endpoints run with the default
  `conflicts=abort`; version conflicts or per-item `failures` surface as an
  execution error including the response body.

---

## 7. Result shape

The handler normalises each backend's native result into a small JSON object at
`output`:

- **SQL insert / upsert**: `{ "rows_affected": n, "returning": [ {..}, .. ] }`
  (`returning` present only where supported, §6.3; single-row MySQL insert also
  carries `"last_insert_id": n`).
- **SQL update / delete**: `{ "rows_affected": n, "returning": [..] }`.
- **MongoDB**: `{ "inserted": n, "matched": n, "modified": n, "deleted": n,
  "upserted_id": <id|null>, "ids": [..] }` (only the fields relevant to the `op`).
- **Elasticsearch**: the same doc-store keys as Mongo — insert →
  `{ "inserted": n, "ids": [..] }`; update → `{ "matched": total, "modified":
  updated }`; delete → `{ "deleted": n }`; upsert → `{ "matched": .., "modified":
  .. }` plus `"upserted_id"` when the document was created.

`rows_to_json` (already used by `db_read`/`data_query`) converts `RETURNING` rows to
JSON, so the read-back path is shared.

---

## 8. Security

- **Always parameterised.** Every `values`/`set` literal and every resolved `param`
  is a bound parameter (SQL, via sea-query-binder) or a document value (Mongo/ES).
  Never string interpolation — identical to the query dialect (query-dialect.md §13).
- **Identifiers only from the envelope and schema.** Table and column names are
  validated against a strict charset and quoted per dialect by sea-query; conflict
  targets come from the declared schema.
- **Write allowlist.** With `UnmappedPolicy::Reject`, only declared entities/columns
  are writable. A new per-column **`writable`** hint on `schema::Column` (default
  `true`, alongside the existing `queryable`) lets the schema mark read-only or
  generated columns (identity/serial, computed) as non-writable; a `values`/`set`
  naming one is a located `WriteError::FieldNotWritable`.
- **Schema is privileged configuration**, authored alongside connectors, never built
  from request input.
- **Per-connector operation gates.** A db/es connector config can disable
  operation types (`operations: { read, insert, update, delete, upsert,
  raw_write }`, all default `true` — `src/connector/config.rs`). A gated call is
  rejected with a validation error naming the op and connector, so a connector
  can be made read-only (or insert-only, …) in config regardless of what a
  workflow asks for. `raw_write` gates the raw-SQL `db_write` escape hatch,
  which cannot be classified per-op.

---

## 9. Config

```rust
// src/config/write.rs
pub struct WriteConfig {
    pub max_rows: u64,          // hard cap on rows per bulk insert (default 1000)
    pub allow_unfiltered: bool, // permit `all: true` unfiltered update/delete (default false)
}
```

Validated at startup (`require_nonzero(max_rows, ..)`), wired into `AppConfig` next
to `query: QueryConfig`, and passed into `DataWriteHandler` in
`build_custom_functions()`. Both keys are surfaced to a query-builder UI (like the
query config) so it can show the row cap and whether unfiltered writes are permitted
before "Run".

---

## 10. Errors

A new located `WriteError` alongside `QueryError`, mapping to
`DataflowError::Validation` (4xx) at the handler edge, exactly as `QueryError` does:

```rust
pub enum WriteError {
    InvalidEnvelope(String),                              // malformed WriteSpec / missing op field
    MissingField { field: String, op: String },          // e.g. `set` absent for update
    UnfilteredMutation { op: String },                    // update/delete with no filter and no `all`
    TooManyRows { requested: u64, max: u64 },             // bulk insert over max_rows
    FieldNotWritable { field: String },                   // column marked non-writable / undeclared (reject mode)
    FeatureUnsupportedByTarget { feature: String, target: String }, // RETURNING on MySQL, ES all, …
}
```

The `filter` reuses `QueryError` verbatim (it is lowered by the query dialect's
`lower_with`), so update/delete filter errors are the same located messages authors
already see from `data_query`.

---

## 11. Phased rollout

Mirrors the query dialect's phasing so each phase is independently shippable and
tested.

- **Phase 1 — SQL INSERT.** `src/query/write.rs` envelope; `render_insert` (single +
  bulk) over `InsertStatement`; `json_to_sea_value` (null-preserving);
  `build_write_for`; `WriteConfig` with `max_rows`; the `data_write` handler for SQL
  connectors; `RETURNING` on PG/SQLite. **De-risk the `InsertStatement` → `AnyPool`
  bind first** (as query Phase 1 did for `SelectStatement` — the binding is the one
  integration unknown). Golden + execution tests.
- **Phase 2 — SQL UPDATE + DELETE.** Reuse `lower_with` + `render_expr` for the
  filter (including relations); the unfiltered-mutation guard (§6.1); `RETURNING`.
- **Phase 3 — Upsert.** SQL `ON CONFLICT` / `ON DUPLICATE KEY`; the conflict-target
  divergence (§6.4).
- **Phase 4 — MongoDB.** insert/update/delete/upsert over the same envelope, reusing
  the Mongo condition renderer; `id`→`_id`; the Mongo result shape (§7).
- **Phase 5 — Elasticsearch (shipped).** `EsWrite` renderer in `backend/es.rs`
  (`_bulk` / `_update_by_query` / `_delete_by_query` / `_update` /
  `_doc?op_type=create`) + live HTTP execution in the handler; forced refresh for
  read-your-writes (§6.6); capability errors for `returning`, bulk upsert,
  non-`_id` conflict targets, and `_id` mutation.
- **Later.** Multi-statement transactions; `UPDATE ... SET col = col + 1`
  (arithmetic in `set`); optimistic concurrency via a version column; `$unset` /
  column removal; MySQL `RETURNING` emulation.

---

## 12. Testing

- **Golden render tests** per backend/op, in `backend/sql.rs`'s existing
  `#[cfg(test)] mod tests` style (assert exact rendered SQL per dialect): single and
  bulk insert; update/delete with a filter (reusing the query filter fixtures);
  upsert per dialect; the unfiltered-mutation rejection; the `max_rows` rejection;
  the MySQL-`RETURNING` capability error.
- **Cross-target equivalence**: the same update `filter` renders a SQL `WHERE` and a
  Mongo filter document meaning the same set of rows — proving the shared filter path.
- **Integration** (`tests/integration/data_write_test.rs`, `common::test_app()` +
  in-memory SQLite): `data_write` insert → `data_query` read-back confirms the row;
  update-with-filter then read-back; delete-with-filter then read-back; unfiltered
  update rejected; over-`max_rows` insert rejected.
- **Cross-backend e2e** (`tests/integration/data_roundtrip_test.rs` + the
  testcontainers harness in `tests/integration/common/backends.rs`): one CRUD
  round-trip (insert → filtered read → update → delete → upsert → auto-id insert),
  written once and run against SQLite, Postgres, MySQL, MongoDB, and
  Elasticsearch (`roundtrip_es` spins up an ephemeral ES container; the envelopes
  carry the `id` → `_id` schema rename). `es_test.rs` covers the ES search path on
  the same harness.
- **Build matrix**: existing `cargo test` stays green (SQLite paths run without
  Docker); the ES write renderer is golden-tested in `backend/es.rs` without a
  live cluster; the container-backed tests run under `-- --ignored` in the
  dedicated CI job.

---

## 13. Open questions

1. **Transactions.** Should `data_write` support a batch of statements (or a
   sequence of tasks) as one atomic unit? SQL and Mongo can; ES cannot. Defer, or
   design a `transaction` envelope now?
2. **Heterogeneous batch.** One call performing mixed ops (insert some, update
   others) — worth a `[ops]` array, or keep one op per task and compose in the
   workflow?
3. **Optimistic concurrency.** An update guarded by a version/`updated_at` column
   (`WHERE id = ? AND version = ?`, bump on write) is a common safe-write pattern —
   is it worth first-class support or left to the `filter`?
4. **Arithmetic in `set`.** `col = col + 1` needs column references in `set` (Mongo
   `$inc`), which the current value model excludes. Add a narrow `{"field": ..}` /
   `{"$inc": n}` form, or leave to `db_write`?
5. **Schema location.** Same open question as the query dialect (query-dialect.md
   §16): inline in the `data_write` input, attached to the connector, or a shared
   `entities` admin resource? A write allowlist makes a *shared, authoritative*
   schema more attractive than inline.
6. **MySQL RETURNING emulation.** Reject (current proposal), or emulate with a
   follow-up `SELECT` inside a transaction?

---

## 14. Decisions taken in this draft

1. **Named `data_write`**, the write peer of `data_query`; `db_write` stays as the
   raw escape hatch (§1).
2. **The update/delete `filter` is the query dialect's filter**, reused verbatim
   (`lower_with` → `Cond` → `render_expr`), including relation predicates — the
   central reuse and the reason the write path is cheap (§4.1).
3. **Parity-or-error throughout**, inherited from the query dialect: unfiltered
   mutations rejected unless explicitly acknowledged (§6.1), bulk over cap rejected
   not truncated (§6.2), `RETURNING`/upsert divergences surfaced as located
   capability errors (§6.3–6.4), never silent approximation.
4. **Same token model and injection-safety** as `data_query`: `field`/`param`/`var`,
   every value bound (§3.2, §8).
5. **Dialect from the connection-string scheme** (`detect_backend`), the same source
   `AnyPool` uses — not `get_backend()`, not the unused `driver` field (§4.2).
6. **ES writes designed now, wired later** (§4.4, Phase 5), matching how the query
   dialect staged ES.
```
