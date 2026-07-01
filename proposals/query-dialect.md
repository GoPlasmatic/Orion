# Proposal: a common query dialect for SQL (sea-query), MongoDB, and Elasticsearch

**Status:** Draft for discussion.
**Layer:** This is an **Orion** feature. datalogic-rs is not involved in query
translation, and dataflow-rs gains nothing new. The whole thing lives in Orion,
alongside the connectors, pools, and drivers it already owns.
**Goal:** Let a workflow express one database-agnostic data-fetch query (a filter
plus projection, sort, and paging) once, in familiar JSONLogic-shaped syntax, and
run it unchanged against a SQL database (via sea-query), MongoDB, or Elasticsearch.
Switching backend is a connector change, not a rewrite.

The centre of this proposal is **the dialect**: a single, backend-neutral
condition model (an intermediate representation, the `Cond` tree) plus the precise
rules for rendering it into each backend's native query. Everything else (the
envelope, the handler, execution) hangs off that.

---

## 1. Why Orion, and why no datalogic

Two earlier drafts (in datalogic-rs) put the translator inside datalogic and the
executor inside dataflow-rs. Both were wrong for this codebase:

- **dataflow-rs does no IO.** It defines typed config schemas and pre-compiles
  JSONLogic, then delegates execution to handlers the host registers. It has no
  DB drivers and no connector layer, by design.
- **Orion already owns everything the query needs.** Connectors
  (`src/connector/config.rs`), pooled clients (`SqlPoolCache` over `sqlx::AnyPool`,
  `MongoPoolCache`), circuit breakers, secrets, and the **sea-query 0.32 +
  sea-query-binder 0.7** dependencies (today used only by Orion's internal storage
  layer, with the `sqlx-any` feature already enabled). The `db_read`, `db_write`,
  and `mongo_read` handlers already live in `src/engine/functions/` and are
  registered into the dataflow engine as custom `AsyncFunctionHandler`s — note they
  run **raw SQL over `sqlx::query`**, so `data_query` will be the first handler to
  assemble SQL with sea-query for an external connector.

The one piece that is genuinely JSONLogic-shaped is the **filter**. But the filter
is never evaluated (it is translated, not run), it has its own semantics
(`{"field":"age"}` means a column, not a context lookup — the dialect uses `field`
where datalogic uses `var`/`val`), and datalogic's compiled AST is the wrong shape
for translation anyway (it is optimised for evaluation).
So the filter shares no machinery with the datalogic engine. Walking a small,
curated JSONLogic subset into a condition tree is a few hundred lines of Orion
code over `serde_json`, sitting next to the renderers that must live in Orion
regardless.

**Decision: build the entire translate-and-execute path in Orion.** datalogic is
not a dependency of this feature. If a non-Orion consumer ever needs the same
translator (a browser query builder over WASM, another service), lift the
`vocab` + `lower` + `ir` trio into a standalone crate then; nothing here blocks
that later.

### A note on sea-query as "the model"

sea-query cannot be the neutral model, because its `Condition`/`Expr` types only
render SQL. They will never emit a MongoDB pipeline or an Elasticsearch query
body. So we still need our own small IR above sea-query, and sea-query becomes one
of three render targets under it. That IR is the dialect this proposal defines.

---

## 2. Where this lands in Orion

New module `src/query/`, plus one new handler and the usual registration edits.

| File | Responsibility |
|------|----------------|
| `src/query/mod.rs` | `QuerySpec`, `translate()`, `validate()`, re-exports |
| `src/query/spec.rs` | envelope parse: `source` / `filter` / `fields` / `sort` / `limit` / `skip` / `include` |
| `src/query/vocab.rs` | the allowed operator set; rejects everything else with a located error |
| `src/query/ir.rs` | the dialect: `Cond`, `FieldRef`, `Value`, `CmpOp`, `TextOp`, `Quant`, `RelRef` |
| `src/query/schema.rs` | `EntityRegistry`: entities, typed columns, relations, identity mode |
| `src/query/lower.rs` | JSONLogic filter JSON to `Cond`; field/relation resolution; semantic normalisation |
| `src/query/shape.rs` | `ResultShape` (renames + include nesting) for hydration |
| `src/query/backend/mod.rs` | `Backend` trait, `Capabilities`, `RenderedQuery` |
| `src/query/backend/sql.rs` | `Cond` + envelope to `sea_query::SelectStatement`; dialect from connector connection-string scheme |
| `src/query/backend/mongo.rs` | `Cond` + envelope to an aggregation pipeline (or a plain find) |
| `src/query/backend/es.rs` | `Cond` + envelope to an ES query body (design now, wire later) |
| `src/query/error.rs` | `QueryError` |
| `src/engine/functions/data_query.rs` | the `data_query` `AsyncFunctionHandler` |

Wiring for the new handler follows the exact pattern used by `db_read` today:

1. `src/engine/functions/mod.rs`: add `pub mod data_query;`
2. `src/engine/mod.rs`: add `"data_query"` to `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS`
3. `src/engine/mod.rs` `build_custom_functions()`: register `DataQueryHandler { pool_cache, mongo_pool_cache, registry }`
4. `src/engine/functions/schema.rs`: add `DATA_QUERY_FIELDS` and a `REGISTRY` entry

The handler reuses the existing connector/execution helpers — `resolve_connector`,
`require_db_connector`, `SqlPoolCache::get_pool` / `MongoPoolCache::get_client`,
`timed_query`, `rows_to_json`, `apply_output`, `extract_output_path`. The net-new
work is `src/query/` **plus** the SQL render-and-bind glue: `db_read`/`db_write`
today run raw `sqlx::query` strings, so `data_query` is the first place a
`sea_query::SelectStatement` is built, rendered for the right dialect, and bound to
`sqlx::AnyPool` via `sea-query-binder`'s `SqlxValues` (the `sqlx-any` feature is
already enabled — see §8; de-risk this binding early, it is the linchpin of
Phase 1).

---

## 3. The common query model

### 3.1 The envelope

```json
{
  "source": "users",
  "filter": {
    "and": [
      { ">":    [ { "field": "age" }, 18 ] },
      { "==":   [ { "field": "status" }, { "param": "status" } ] },
      { "some": [ { "field": "orders" }, { ">": [ { "field": "total" }, 100 ] } ] }
    ]
  },
  "fields": ["id", "name", "email"],
  "sort":   [ { "created_at": "desc" } ],
  "limit":  50,
  "skip":   0,
  "include": { "orders": { "fields": ["id", "total"], "limit": 5 } }
}
```

| Field | Meaning |
|-------|---------|
| `source` | logical entity; maps to a table / collection / index |
| `filter` | the JSONLogic condition, including relation predicates (the dialect) |
| `fields` | bare column names to return; empty means all |
| `sort` | ordering keys, each `{ "<field>": "asc" \| "desc" }` |
| `limit` / `skip` | paging; an omitted `limit` defaults to a configurable page size (100) and every query is capped by a configurable hard maximum (§5.12) |
| `include` | related records to nest in the result |

Only `filter` is JSONLogic-shaped. Inside it, `{"field": "..."}` names a column
(the dialect's own token, replacing datalogic's `var`/`val`) and `{"param": "..."}`
references a value supplied out-of-band in the `params` map (§3.4). The rest is a
plain envelope, parsed by `src/query/spec.rs` and assembled by each backend around
the rendered condition.

### 3.2 The condition core (the dialect IR)

```rust
// src/query/ir.rs
pub enum Cond {
    True,
    False,
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    Compare { field: FieldRef, op: CmpOp, value: Value },
    In      { field: FieldRef, values: Vec<Value>, negated: bool },
    IsNull  { field: FieldRef, negated: bool },
    Between { field: FieldRef, low: Value, high: Value, low_incl: bool, high_incl: bool, negated: bool },
    Text    { field: FieldRef, op: TextOp, pattern: String, ci: bool }, // ci = case-insensitive
    Rel     { quant: Quant, rel: RelRef, cond: Box<Cond> }, // some / all / none over a declared relation
}

pub enum CmpOp  { Eq, Ne, Lt, Le, Gt, Ge }
pub enum TextOp { StartsWith, EndsWith, Contains }
pub enum Quant  { Any, All, None } // Any = `some`
```

`Between` and `Text` are kept as first-class nodes rather than desugared, because
every backend has a dedicated, more efficient form (`BETWEEN`, `range`, `$gte/$lte`;
`LIKE`, `prefix`/`wildcard`, `$regex`). `Between` carries per-bound inclusivity
(`low_incl`/`high_incl`) so a chained `{"<": [a, {"field":"x"}, b]}` (strict) and a
chained `{"<=": [...]}` (inclusive) render faithfully — native SQL `BETWEEN` is used
only when both bounds are inclusive, otherwise explicit `>`/`>=` + `<`/`<=` (see
§4.1 and §5.11). `IsNull`, `In`, and the negated flags exist so the renderer can
pick the native negation instead of wrapping in `Not`.

### 3.3 Field references and types

```rust
pub struct FieldRef {
    pub path: Vec<String>,      // dotted path as written, e.g. ["address","city"]
    pub physical: String,       // resolved physical column / field name (rename-aware)
    pub ty: FieldType,          // drives coercion and ES exact-vs-text selection
}

pub enum FieldType {
    Bool, Int, Float, Decimal, Text, Keyword, Date, Timestamp, Json, Unknown,
}
```

`ty` is the quiet workhorse of cross-backend fidelity. It decides date coercion,
numeric-vs-string binding, and (for ES) whether a `==` becomes a `term` on a
keyword field or a `match` on an analysed field. Without a schema, `ty` is
`Unknown` and the renderer uses each backend's untyped default. This is the honest
boundary of "switch backend, not a rewrite": full fidelity for typed values (dates,
decimals, numbers-bound-as-strings, ES term-vs-match) **requires the schema**; the
zero-config path is best-effort per backend and can diverge on typed comparisons
(e.g. a date literal compared as a string works on SQL via implicit cast but not
against a Mongo `ISODate`).

### 3.4 Values and params

```rust
pub enum Value { Null, Bool(bool), Int(i64), Float(f64), Str(String), List(Vec<Value>) }
```

A literal in the filter becomes a `Value` directly. Because `{"field": ...}` is
reserved for columns inside the filter, there is no way to write a message-context
value there — which is exactly why `params` exists: a `{ "param": "x" }` node in the
filter is resolved to a `Value` **before** lowering, from the handler's `params`
map (see §12). Inside that `params` map, the ordinary datalogic `{ "var": "..." }`
keeps its usual meaning (a lookup into the message context), so the three tokens
never collide: `field` = column (filter only), `param` = named value reference
(filter only), `var` = context lookup (`params` map only). Params never reach the
rendered query as placeholders of their own; they are folded into literals, which
then become bound parameters (SQL) or inline document values (Mongo/ES). This keeps
every value parameter-bound and injection-safe.

### 3.5 The vocabulary, and what is rejected

`src/query/vocab.rs` accepts exactly this set, mirroring the earlier design:

| Category | JSONLogic | Notes |
|----------|-----------|-------|
| Comparison | `==` `!=` `<` `<=` `>` `>=` | `===`/`!==` treated as `==`/`!=` |
| Logical | `and` `or` `!` | empty `and` is true, empty `or` is false |
| Membership | `in` with a **list** 2nd operand: `{"in":[{"field":"status"},["a","b"]]}` | field in list → `IN` / `$in` / `terms` |
| Null | `==`/`!=` vs `null`, and `missing` | `exists` is dropped (not a baseline datalogic op in Orion); see §5.3 |
| Range | chained `{ "<": [a, {"field":"x"}, b] }` (strict) or `{ "<=": [...] }` (inclusive) | `BETWEEN` (inclusive only) / `$gte`+`$lte` / `range`; strict bounds render as explicit `>`/`<` — see §5.11 |
| Text (substring) | `in` with a **string/field** 2nd operand: `{"in":["smith",{"field":"name"}]}` | datalogic's `[needle, haystack]` order → `LIKE '%smith%'` / `$regex` / `wildcard`; capability-gated (§5.4) |
| Text (prefix/suffix) | `starts_with` `ends_with` | e.g. `{"starts_with":[{"field":"name"},"sm"]}`; capability-gated (§5.4) |
| Field ref | `field` | column, JSON path, or relation path (replaces datalogic `var`/`val`) |
| Parameter | `param` | resolved before lowering (§3.4) |

Anything else (`map`, `reduce`, `cat`, arithmetic as a value, custom operators,
`try`/`throw`) is a `QueryError::UnsupportedInQuery { op, at }`, naming the operator
and its location. Column-to-column comparison (`{"<":[{"field":"a"},{"field":"b"}]}`)
is rejected in v1: it needs Mongo `$expr` and has no portable form yet.

**On `in` (deliberately kept datalogic-faithful).** `in` is a single operator with
two meanings, disambiguated exactly as datalogic-rs disambiguates it — by the
**second (haystack) operand**: a list means membership (`field IN (...)`), a string
or field means substring. The operand order is datalogic's `[needle, haystack]`, so
`{"in":[{"field":"status"},["a","b"]]}` is "status ∈ {a,b}" while
`{"in":["smith",{"field":"name"}]}` is "'smith' is a substring of name". This
overloading is direction-sensitive; it is retained (rather than split into a
separate `contains` operator) to stay identical to the JSONLogic Orion already
evaluates, and the direction rule is pinned by golden tests.

**Field scoping inside `some`/`all`/`none`.** At the top level a `field` names a
root-entity column. Inside a relation predicate it names a column of the **related**
entity — `{"some":[{"field":"orders"},{">":[{"field":"total"},100]}]}` resolves
`total` to `orders.total`. v1 does not allow a relation predicate to reference an
arbitrary root column; the only correlation is the declared join key, applied by the
renderer. Referencing a root column inside the predicate is a located
`NotRepresentable` error.

---

## 4. The dialect: backend mapping

This is the core. One `Cond` tree renders three ways. SQL is expressed through
sea-query (so the SQL text and parameter style are the dialect builder's job, not
ours); MongoDB is an aggregation-pipeline `$match` document (or a plain find
filter); Elasticsearch is a query DSL body evaluated in filter context (no
scoring).

### 4.1 Scalar and logical (the master table)

| `Cond` | SQL (sea-query) | MongoDB (`$match`) | Elasticsearch (filter context) |
|--------|-----------------|--------------------|--------------------------------|
| `True` | `Cond::all()` (empty AND) | `{}` | `{ "match_all": {} }` |
| `False` | `Expr::val(1).eq(0)` | `{ "$expr": false }` | `{ "match_none": {} }` |
| `And([..])` | `Cond::all()` + each | `{ "$and": [..] }` | `{ "bool": { "filter": [..] } }` |
| `Or([..])` | `Cond::any()` + each | `{ "$or": [..] }` | `{ "bool": { "should": [..], "minimum_should_match": 1 } }` |
| `Not(c)` | `c.not()` | `{ "$nor": [c] }` | `{ "bool": { "must_not": [c] } }` |
| `Compare Eq` | `Expr::col(f).eq(v)` | `{ f: { "$eq": v } }` | `{ "term": { f: v } }` |
| `Compare Ne` | `Expr::col(f).ne(v)` | `{ f: { "$ne": v } }` | `{ "bool": { "must_not": { "term": { f: v } } } }` |
| `Compare Lt/Le/Gt/Ge` | `.lt/.lte/.gt/.gte(v)` | `{ f: { "$lt/$lte/$gt/$gte": v } }` | `{ "range": { f: { "lt/lte/gt/gte": v } } }` |
| `In` (not negated) | `Expr::col(f).is_in(vs)` | `{ f: { "$in": vs } }` | `{ "terms": { f: vs } }` |
| `In` (negated) | `.is_not_in(vs)` | `{ f: { "$nin": vs } }` | `{ "bool": { "must_not": { "terms": { f: vs } } } }` |
| `In` (empty list) | `False` (renders `1 = 0`) | `{ "$expr": false }` | `{ "match_none": {} }` |
| `IsNull` (not negated) | `Expr::col(f).is_null()` | `{ f: { "$eq": null } }` | `{ "bool": { "must_not": { "exists": { "field": f } } } }` |
| `IsNull` (negated) | `.is_not_null()` | `{ f: { "$ne": null } }` | `{ "exists": { "field": f } }` |
| `Between` (both bounds inclusive) | `Expr::col(f).between(a, b)` | `{ f: { "$gte": a, "$lte": b } }` | `{ "range": { f: { "gte": a, "lte": b } } }` |
| `Between` (either bound strict) | `col >[=] a AND col <[=] b` (per-bound) | `{ f: { "$gt/$gte": a, "$lt/$lte": b } }` (per-bound) | `{ "range": { f: { "gt/gte": a, "lt/lte": b } } }` (per-bound) |
| `Text StartsWith` | `.like("v%")` (escaped) | `{ f: { "$regex": "^v" } }` | `{ "prefix": { f: { "value": "v" } } }` |
| `Text EndsWith` | `.like("%v")` | `{ f: { "$regex": "v$" } }` | `{ "wildcard": { f: "*v" } }` |
| `Text Contains` | `.like("%v%")` | `{ f: { "$regex": "v" } }` | `{ "wildcard": { f: "*v*" } }` |

Empty `In` folds to `False` at lowering (§5.5); the table lists it for clarity.

### 4.2 Relations: `some` / `all` / `none`

A relation predicate filters the root entity by a condition on a related entity. It
is always an `EXISTS`-style semi/anti join, so the root row count never changes: no
duplicate rows, no `DISTINCT`, no `GROUP BY`. Two sibling `some`s become two
independent `EXISTS`, never a cross product.

The mapping depends on how the relation is stored (declared in the schema, §11):

| `Rel` | SQL | MongoDB | Elasticsearch |
|-------|-----|---------|---------------|
| `Any` (embedded) | `EXISTS (SELECT 1 FROM rel WHERE <join> AND c)` | `{ rel: { "$elemMatch": c } }` | `{ "nested": { "path": rel, "query": c } }` |
| `Any` (referenced) | correlated `EXISTS` on the foreign table | `$lookup` + `$match` pipeline stages | `{ "has_child": { "type": rel, "query": c } }` |
| `None` | `NOT EXISTS (...)` | `{ rel: { "$not": { "$elemMatch": c } } }` | `{ "bool": { "must_not": <Any> } }` |
| `All` | `EXISTS(...) AND NOT EXISTS(... AND (NOT c OR c IS NULL))` | `$elemMatch` non-empty + `$not $elemMatch` of the negation | limited; capability error unless opted in |
| `rel.field` (to-one) | correlated scalar subquery / JOIN | `$lookup` (referenced) or embedded field | nested / parent field |

The MongoDB renderer emits an **aggregation pipeline** whenever a referenced
relation is present (so `$lookup` can join collections), and a plain `find` filter
otherwise. The `RenderedQuery::Mongo` variant therefore always carries a pipeline;
the executor uses `.find()` for the single-`$match` case and `.aggregate()`
otherwise.

### 4.3 The envelope around the condition

| Envelope | SQL (sea-query) | MongoDB | Elasticsearch |
|----------|-----------------|---------|---------------|
| `fields` | `.column(..)` per field, or `Asterisk` | `$project` / find projection | `_source` includes |
| `sort` | `.order_by_with_nulls(col, ord, null_ord)` | `$sort` / `.sort()` | `sort` with `missing` |
| `limit` | `.limit(n)` | `$limit` / `.limit()` | `size` |
| `skip` | `.offset(n)` | `$skip` / `.skip()` | `from` |
| `include` | correlated JSON aggregation or second query | `$lookup` / embedded array | nested `inner_hits` / child docs |

---

## 5. Dialect divergence analysis and parity rules

Translating one vocabulary to three engines means their edge cases differ. The
governing rule is **parity-or-error**: match the reference semantics where a
backend can, and raise a precise, located capability error where it cannot. Never
approximate silently. Each subsection states the divergence and the dialect's fixed
rule.

### 5.1 Constants and empty logical groups

An empty `and` is `True`; an empty `or` is `False`. These are folded at lowering so
renderers never see an empty `And`/`Or`. `True`/`False` still need real forms
because they can appear as an operand: SQL uses `1 = 0` for false and an empty
`Cond::all()` for true, MongoDB uses `{ "$expr": false }` and `{}`, Elasticsearch
uses `match_none` and `match_all`.

### 5.2 Boolean context: filter vs query (Elasticsearch)

ES distinguishes scoring queries from non-scoring filters. Every condition this
dialect produces is a boolean predicate, so the ES renderer emits everything in
**filter context** (`bool.filter` / `bool.must_not`), never `must` with scoring.
`or` becomes `should` with `minimum_should_match: 1`. This keeps ES results
set-equivalent to SQL and Mongo rather than relevance-ranked.

### 5.3 Null versus missing

SQL columns always exist, so `IS NULL` means "present but null". MongoDB
`{ f: { "$eq": null } }` matches both null **and** absent. Elasticsearch has no
queryable stored null at all; it only knows exists / not-exists. These cannot be
reconciled field-by-field, so the dialect **defines `IsNull` to mean "no meaningful
value" (null or absent)** uniformly:

- SQL: `col IS NULL` (only null is possible, which satisfies the definition).
- Mongo: `{ f: { "$eq": null } }` (null or missing, matching the definition).
- ES: `must_not exists` (absent, the only thing ES can express).

This is documented behaviour, not an accident. A workflow that needs SQL's stricter
"present-and-null" is outside the portable vocabulary. `IsNull` is produced by
`{"==":[{"field":"x"}, null]}` (negated by `!=`) or by `missing`; there is no
`exists` operator (it is not a baseline datalogic operator in Orion, and `!= null` /
negated `missing` already cover "has a value").

### 5.4 Text match: case and analysis

`LIKE`, Mongo `$regex`, and ES text queries differ in case-sensitivity, collation,
and (for ES) the field analyzer. Text predicates come from `starts_with`/`ends_with`
and the substring form of `in` (§3.5). The dialect carries an explicit `ci`
(case-insensitive) flag on `Text` and renders:

- **SQL**: default case-sensitive `LIKE` with the pattern wildcard-escaped (as
  Orion's storage layer already escapes `\`, `%`, `_` for LIKE). For `ci`, use
  Postgres `ILIKE`, and `LOWER(col) LIKE LOWER(pattern)` on MySQL/SQLite. Note the
  collation caveat: MySQL's default collation is case-insensitive, so exact
  case-sensitive `LIKE` there needs a binary collation; this is called out, not
  silently ignored.
- **Mongo**: `$regex` with `^`/`$` anchors; `ci` adds `$options: "i"`.
- **Elasticsearch**: `prefix` / `wildcard` with `case_insensitive: true` when `ci`
  (ES 7.10+). Analyzer-dependent full `match` is **not** substituted for these; it
  is capability-gated, because its results depend on the field's analyzer and are
  not set-equivalent to a substring predicate.

`ci` defaults to case-sensitive in v1 (datalogic's `in`/`starts_with`/`ends_with`
carry no case flag). A per-predicate surface toggle, or deriving `ci` from a
declared column collation in the schema, is a later addition.

### 5.5 Empty membership

An empty `in` list renders to a constant `False` (`1 = 0` on SQL), never the syntax
error `col IN ()`. A negated empty `in` renders to `True`.

### 5.6 `all` and three-valued logic (SQL)

`all` follows the reference semantics: false on an empty relation, and a null inner
predicate counts as a counterexample. A naive SQL anti-join
`EXISTS(...) AND NOT EXISTS(... AND NOT c)` drops null-predicate rows under
three-valued logic, so it is too permissive. The SQL renderer emits the anti-join as
`NOT EXISTS(... AND (NOT c OR c IS NULL))` to restore parity. A golden test pins
this. `all` is supported on SQL, supported on Mongo with care, and **capability-
limited on ES** (nested "all match" needs `must_not` of the negation with
empty-relation caveats); ES raises `FeatureUnsupportedByTarget` unless an explicit
opt-in is set.

### 5.7 Sort and null ordering

SQL (`NULLS FIRST/LAST`), Mongo (nulls sort low), and ES (`missing: _first/_last`)
order nulls differently. v1 normalises to one documented default: **nulls last on
`asc`, nulls first on `desc`**. SQL emits explicit `NULLS` clauses via
`order_by_with_nulls`; note MySQL has no `NULLS FIRST/LAST` syntax and needs an
`ISNULL(col)` prefix key to emulate it, which the SQL renderer adds for the MySQL
driver. ES sets `missing` accordingly; Mongo documents the default. Per-key
`nulls: first | last` is an open question (§16).

### 5.8 Deep pagination (Elasticsearch)

`skip`/`limit` map to ES `from`/`size`, bounded by `index.max_result_window`
(~10k). Beyond it the ES renderer raises a capability error rather than returning a
truncated page. `search_after` cursor paging is a later addition.

### 5.9 Referenced relations require aggregation (MongoDB)

An embedded relation is a `find` filter (`$elemMatch`); a referenced relation needs
`$lookup`, which only exists in the aggregation pipeline. The Mongo renderer detects
referenced relations and switches from find to aggregate. This is invisible to the
query author.

### 5.10 Column-to-column comparison

Excluded in v1 across all backends (Mongo needs `$expr`). Rejected at lowering with
a located `NotRepresentable` error rather than mistranslated.

### 5.11 Range inclusivity (chained comparisons)

datalogic evaluates a chained comparison by applying the operator to each adjacent
pair, so `{"<": [a, {"field":"x"}, b]}` means `a < x AND x < b` (**strict** both
ends) and `{"<=": [...]}` means inclusive both ends. The dialect preserves this: a
chained `<` lowers to `Between { low_incl: false, high_incl: false }`, a chained
`<=` to `Between { low_incl: true, high_incl: true }`. SQL emits native `BETWEEN`
only when both bounds are inclusive (SQL `BETWEEN` is inclusive-only); a strict bound
renders as an explicit `>` / `<`. Mongo and ES pick `$gt`/`$gte` and `gt`/`gte` per
bound. A single chained operator cannot express a mixed bound (`a <= x < b`); write
that as an `and` of two comparisons, which lowers to two `Compare` nodes (the SQL
renderer may still fold an inclusive/inclusive pair back to `BETWEEN`). A golden test
pins strict-vs-inclusive so the earlier bug — collapsing a strict `<` into an
inclusive `BETWEEN` — cannot recur.

### 5.12 Default and maximum page size

`limit` is bounded on both sides by Orion app config (a `[query]` section):

- **Default.** When a query omits `limit`, the renderer applies
  `query.default_limit` (default **100**). No query is ever unbounded by accident.
- **Hard maximum.** Every `data_query` is capped by a configured `query.max_limit`.
  A query whose `limit` exceeds it is **rejected** at validation with a located
  `QueryError::LimitExceeded { requested, max }` (parity-or-error: we do not silently
  clamp, so the author knows they never asked for a truncated page). The cap is
  enforced in `validate()` as well as `translate()`, so a query that validates clean
  cannot exceed the cap at run time.

Both keys are read from app config and surfaced to any query-builder UI via
`validate()`, so the UI can show the ceiling before "Run".

---

## 6. Capabilities and validate

```rust
pub struct Capabilities {
    pub relation_all: bool,        // ES: false (unless opted in)
    pub referenced_relations: bool,
    pub text_ci: bool,             // case-insensitive text
    pub deep_pagination: bool,     // ES: bounded by max_result_window
    pub column_to_column: bool,    // false everywhere in v1
}
```

`translate()` lowers and renders. `validate()` runs the same parse, lower, and
capability checks against the chosen backend and discards the rendered output, so a
query that validates clean cannot then fail in translate. The admin API and any
query-builder UI call `validate()` to enable or disable "Run" and to show, inline,
why a query is not runnable on the chosen backend. Most backends support the full
scalar vocabulary; `Capabilities` exists to gate the genuine gaps (ES `all`,
analyzer-dependent text, deep paging).

---

## 7. The Backend trait and execution

```rust
// src/query/backend/mod.rs
pub trait Backend {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn render(&self, q: &LoweredQuery) -> Result<RenderedQuery, QueryError>;
}

pub enum RenderedQuery {
    Sql   { statement: sea_query::SelectStatement, dialect: SqlDialect },
    Mongo { database: String, collection: String, pipeline: Vec<mongodb::bson::Document> },
    Es    { index: String, body: serde_json::Value },
}
```

`LoweredQuery` is the neutral `Cond` plus the resolved envelope and `ResultShape`.
The `data_query` handler picks the backend from the connector type, calls `render`,
then executes on the pool it already knows how to obtain:

- **SQL**: build `(sql, values)` from the `SelectStatement` using the dialect
  derived from the connector's connection-string scheme (see §8), bind the
  `sea-query-binder` `SqlxValues` to the `AnyPool` from `SqlPoolCache`, then run and
  convert with `rows_to_json`.
- **Mongo**: run `pipeline` via `MongoPoolCache::get_client(..)`, using `.find()`
  for a single `$match` and `.aggregate()` when a `$lookup` is present.
- **ES**: deferred until an ES connector and client exist (§10).

---

## 8. SQL rendering with sea-query

The condition maps directly onto the sea-query API the repo already uses:

```rust
// Cond -> sea_query::Condition (sketch)
Cond::And(cs)  => cs.iter().fold(Condition::all(), |c, x| c.add(render(x)));
Cond::Or(cs)   => cs.iter().fold(Condition::any(), |c, x| c.add(render(x)));
Cond::Not(c)   => render(c).not();
Cond::Compare { field, op: Gt, value } => Expr::col(col(field)).gt(lit(value)).into(),
Cond::In      { field, values, negated: false } => Expr::col(col(field)).is_in(lits(values)).into(),
Cond::IsNull  { field, negated: false } => Expr::col(col(field)).is_null().into(),
Cond::Between { field, low, high, negated: false } => Expr::col(col(field)).between(lit(low), lit(high)).into(),
Cond::Text    { field, op: StartsWith, pattern, ci } => like_expr(field, format!("{}%", esc(pattern)), ci),
```

The full statement uses `Query::select().column(..).from(..).cond_where(root).order_by_with_nulls(..).limit(..).offset(..)`, exactly the builder style in `storage/repositories/`.

**One important correction to the existing pattern.** Orion's internal
`build_sqlx()` (in `storage/mod.rs`) picks the dialect from Orion's own storage
backend via `get_backend()`. A `data_query` runs against an **external** connector
whose engine may differ from Orion's storage, so `get_backend()` is the wrong
source. The connector *does* carry a `driver` field, but today it is unused and
unvalidated (free-form, defaults to `"postgres"`), while the real driver is the one
`sqlx::AnyPool` infers from the **connection-string scheme** — the same detection
Orion's storage layer already does (`sqlite:` / `postgres://` / `mysql://` /
`mariadb://`). To guarantee the rendered SQL dialect matches the pool that will
execute it, the SQL backend derives the dialect from that scheme — not from
`get_backend()` and not from the `driver` field:

```rust
fn dialect_from_url(conn: &str) -> SqlDialect {
    // reuse Orion's existing scheme detection (storage/mod.rs)
    if conn.starts_with("postgres://") || conn.starts_with("postgresql://") {
        SqlDialect::Postgres
    } else if conn.starts_with("mysql://") || conn.starts_with("mariadb://") {
        SqlDialect::MySql
    } else {
        SqlDialect::Sqlite
    }
}

fn build_for(dialect: SqlDialect, stmt: &mut SelectStatement) -> (String, SqlxValues) {
    match dialect {
        SqlDialect::Postgres => stmt.build_sqlx(PostgresQueryBuilder),
        SqlDialect::MySql    => stmt.build_sqlx(MysqlQueryBuilder),
        SqlDialect::Sqlite   => stmt.build_sqlx(SqliteQueryBuilder),
    }
}
```

The resulting `SqlxValues` bind to `AnyPool` through `sea-query-binder`'s `sqlx-any`
feature (already enabled in `Cargo.toml`). Because no existing handler exercises
this sea-query → `AnyPool` path, prove it with a one-query spike before building out
the renderer — it is the single integration unknown in Phase 1 (cross-driver value
coercion for `i64`/decimal/date in particular).

`EXISTS` for relations is a correlated sub-select built with a second
`Query::select()` whose `WHERE` carries the join predicate plus the lowered inner
condition; the exact sea-query 0.32 constructor for `EXISTS` is an implementation
detail to confirm at build time, but the target SQL shape is fixed by §4.2 and §5.6.

---

## 9. MongoDB rendering

The condition maps onto the BSON forms in the master table. Values become BSON via
the same `bson::to_document` path `mongo_read` already uses. The renderer walks the
`Cond` tree building a `$match` document, and prepends `$lookup` stages for any
referenced relation. `$sort` / `$skip` / `$limit` / `$project` follow. When the
pipeline is a single `$match` with no `$lookup`, the executor unwraps it to a plain
`.find(filter).sort().skip().limit().projection()` for efficiency; otherwise it runs
`.aggregate(pipeline)`.

`id` to `_id` renaming and any other logical-to-physical field names come from the
schema (§11) and are applied during lowering, so the renderer sees physical names.

---

## 10. Elasticsearch rendering (design now, wire later)

The ES mapping is fully specified in §4 and §5, but **Orion currently has no
Elasticsearch dependency, connector, or client** (confirmed: no `elastic*` anywhere
in the repo). Implementing the ES path therefore requires, in a later phase:

1. a new `EsConnectorConfig` in `src/connector/config.rs` and a client cache
   alongside `SqlPoolCache` / `MongoPoolCache`;
2. an ES client dependency (for example the official `elasticsearch` crate);
3. wiring the `Es` arm of `RenderedQuery` execution in the handler.

Until then `es.rs` exists as the renderer (pure `Cond` to JSON, unit-testable with
golden files) but is not reachable from a live connector. This keeps the dialect
honest and complete without pretending an ES backend is deployed.

---

## 11. Schema and entity registry

Zero configuration is the default: `source` is the table/collection/index name and
a `field` path is the column name, with `FieldType::Unknown` (see §3.3 for the
fidelity limits of the zero-config path). A schema adds, only when
wanted: renames (logical to physical), type hints (so `in` / null / dates / ES
term-vs-match translate correctly), an allowlist (only declared fields are
queryable), per-backend names (`id` to `_id`), and the relation declarations that
`some`/`all`/`none` require.

```rust
// src/query/schema.rs
pub struct EntityRegistry { pub entities: HashMap<Box<str>, Entity>, pub unmapped: UnmappedPolicy }
pub struct Entity   { pub physical: Box<str>, pub columns: HashMap<Box<str>, Column>, pub relations: HashMap<Box<str>, Relation> }
pub struct Column   { pub name: Box<str>, pub ty: FieldType, pub queryable: bool }
pub struct Relation {
    pub to: Box<str>, pub kind: Cardinality,       // HasOne | HasMany | ManyToMany
    pub local: Box<str>, pub foreign: Box<str>,
    pub through: Option<Junction>,                 // M:M
    pub mongo: MongoStorage,                        // Embedded | Referenced
    pub es: EsStorage,                              // Nested | Child
}
pub enum UnmappedPolicy { Reject, Identity }        // Identity = field name is the physical name
```

A relation filter requires the relation to be declared; `some`/`all`/`none` over an
unknown relation is a clear `UnknownRelation` error. The schema is privileged
configuration, authored alongside connectors, never built from end-user input. It
doubles as the UI contract: a query builder reads entities, typed columns, and
relations to offer field pickers, relation pickers, and per-target operator
availability from the same `Capabilities`.

Where the schema is stored is an open question (§16): inline in the `data_query`
input, attached to the connector, or a new `entities` admin resource.

---

## 12. The `data_query` function

A new `AsyncFunctionHandler`, registered exactly like `db_read`.

```json
{ "name": "data_query",
  "input": {
    "connector": "orders_db",
    "query": {
      "source": "users",
      "filter": { "some": [ {"field":"orders"}, {">":[{"field":"total"}, {"param":"min"}]} ] },
      "fields": ["id","name"],
      "include": { "orders": { "fields": ["id","total"] } },
      "limit": 50
    },
    "params": { "min": { "var": "data.threshold" } },
    "output": "data.users"
  } }
```

Note the token split from §3.4: inside `query.filter`, `field` names a column and
`param` references a value; inside `params`, `var` is the ordinary datalogic context
lookup. If `limit` were omitted it would default to `query.default_limit` (100), and
any `limit` above `query.max_limit` is rejected (§5.12).

Execution:

1. Parse the envelope (`src/query/spec.rs`).
2. Resolve `params` against the message context and fold them into the filter as
   literals. v1 resolves simple `{ "var": "..." }` lookups from `ctx`; richer
   JSONLogic param expressions can reuse the engine later. This is the only place
   the message context touches the query, and it produces literals, not SQL.
3. Lower the filter to `Cond`, resolving fields and relations via the schema
   (`src/query/lower.rs`), applying the §5 normalisations.
4. Pick the backend from the connector type, `render`, execute on the pool, convert
   rows/docs to JSON with the existing helpers.
5. Hydrate into the nested logical shape using `ResultShape` (renames plus include
   nesting), then `apply_output(ctx, output, result)`.

`ResultShape` is needed because `fields` may be renamed and `include` nests, so the
raw backend rows cannot be reshaped from the schema alone at read time:

```rust
pub struct ResultShape {
    pub columns:  Vec<(Box<str>, Box<str>)>,                 // logical field -> physical, ordered
    pub includes: Vec<(Box<str>, Cardinality, ResultShape)>, // nested related selections
}
```

`db_read` and `db_write` stay exactly as they are: the raw-SQL escape hatch for
anything outside the portable vocabulary.

---

## 13. Security

- **Always parameterised.** Every literal and resolved param is a bound parameter
  (SQL, via sea-query-binder) or a value in the query document (Mongo/ES). Never
  string interpolation.
- **Identifiers come only from the query and the schema**, validated against a
  strict charset and quoted by sea-query per dialect; join keys come from declared
  relations only.
- **Allowlist.** With `UnmappedPolicy::Reject`, only declared fields and relations
  are reachable, so a filter cannot name an undeclared column.
- The schema is privileged configuration, never derived from request input.

---

## 14. Phased rollout

- **Phase 1, scalar SQL.** `src/query/` with the envelope, the `Cond` IR, the
  vocabulary and lowering (comparison, `and`/`or`/`!`, `in`, null, range with
  faithful strict/inclusive bounds, text), identity mode, config-driven
  `default_limit`/`max_limit` enforcement (§5.12), and the SQL backend over
  sea-query with connection-string-scheme dialect selection. `ResultShape` for
  projection and renames. The `data_query` handler for SQL connectors. Prove the
  sea-query → `AnyPool` binding first (§8). Golden tests.
- **Phase 2, relations (SQL).** Schema relations; `some`/`all`/`none` to
  `EXISTS`/`NOT EXISTS`; to-one dotted paths; M:M via junction; the §5.6 null fix;
  the fan-out-free guarantee.
- **Phase 3, MongoDB.** The document renderer over the same IR; `$elemMatch`
  (embedded) and `$lookup` (referenced); find-vs-aggregate selection; `id` to `_id`.
- **Phase 4, `include`.** Nested projection (JSON aggregation on SQL,
  `$lookup`/embedded on Mongo) and result hydration.
- **Phase 5, Elasticsearch.** Add the ES connector, client, and pool (§10), then
  wire the already-written ES renderer; honest capability errors where `all` and
  analyzer-dependent text are not expressible.
- **Later.** Column-to-column (`$expr`); cursor paging (`search_after`);
  per-key null ordering; aggregation envelope; more targets.

---

## 15. Testing

- **Golden tests** per backend: `QuerySpec` + schema to expected sea-query SQL and
  params / Mongo pipeline / ES body / `QueryError`. Cover each scalar operator, the
  relation filters per storage kind, null handling, empty `in`, `LIKE` escaping, the
  §5.6 `all`/null fix, and the capability errors (ES `all`, deep paging).
- **Cross-target equivalence**: the same relation filter renders SQL `EXISTS`, a
  Mongo `$elemMatch`/`$lookup`, and (in golden form) an ES `nested`/`has_child`, all
  meaning the same set of root rows.
- **Execution smoke test**: a `data_query` task with a relation filter against
  in-process SQLite and a test MongoDB returns equivalent root rows with only the
  connector changed.
- **Build matrix**: the existing `cargo test` stays green; the ES renderer is
  unit-tested via golden files without a live cluster.

---

## 16. Open questions

1. **Schema location.** Inline in `data_query` input, attached to the connector, or
   a new `entities` admin resource with its own table and API?
2. **Param resolution depth.** v1 resolves `{ "var": "..." }` lookups from context.
   Do we want full JSONLogic param expressions (reusing the engine dataflow already
   embeds) in Phase 1, or defer?
3. **`include` on SQL.** Correlated JSON aggregation (`json_agg` / `JSON_ARRAYAGG`)
   versus a second hydration query. One default per driver?
4. **ES `all`.** Reject with a capability error, or offer a documented script-based
   approximation behind an explicit opt-in?
5. **Relation depth.** How deep may relation filters and `include` nest before we
   cap it, to bound query complexity?
6. **Null ordering.** Ship the fixed default (§5.7) or expose per-key
   `nulls: first | last`?
7. **ES timing.** Is the ES connector wanted soon enough to bring Phase 5 forward,
   or is SQL + Mongo the near-term target with ES kept as design only?
8. **Standalone reuse.** If a non-Orion consumer (browser query builder) appears,
   extract `vocab` + `ir` + `lower` into a crate. Worth structuring the module
   boundary for that from day one, or cross that bridge later?

---

## 17. Decisions taken in this revision

Settled after the first round of review (grounded against the current codebase and
datalogic-rs 5.0's actual operator set):

1. **Column reference is `field`, not `var`.** `field` names a column inside the
   filter (replacing datalogic's `var`/`val`), which frees `var` to keep its normal
   context-lookup meaning in the `params` map and removes the earlier triple-meaning
   ambiguity (§3.1, §3.4).
2. **`in` stays datalogic-faithful and overloaded** (membership vs substring by the
   haystack operand), rather than splitting out a separate `contains`, so the query
   surface matches the JSONLogic Orion already evaluates. `starts_with`/`ends_with`
   remain the prefix/suffix operators. Direction rules are pinned by golden tests
   (§3.5).
3. **Range renders strict-vs-inclusive faithfully** via per-bound flags on
   `Between`; native SQL `BETWEEN` only for inclusive/inclusive, explicit `>`/`<`
   otherwise (§3.2, §4.1, §5.11).
4. **`limit` is config-bounded.** `query.default_limit` = 100 when omitted; every
   `data_query` is capped by `query.max_limit`, and exceeding it is a rejected
   `LimitExceeded` error, not a silent clamp (§5.12).
5. **The handler is named `data_query`** — it spans SQL, Mongo, and later ES, so
   "data" rather than "db".
6. **SQL dialect is derived from the connector's connection-string scheme**, the
   same source `AnyPool` uses — not `get_backend()` and not the currently-unused
   `driver` field (§7, §8).

Corrections folded in from codebase verification: `db_read`/`db_write` run raw
`sqlx` (not sea-query), so the SQL render-and-bind path is net-new and the
sea-query → `AnyPool` binding is de-risked first (§2, §8); the fidelity limit of the
zero-config path is stated honestly (§3.3); `exists` is dropped as it is not a
baseline datalogic operator in Orion (§3.5, §5.3).
