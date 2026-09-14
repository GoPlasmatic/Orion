<!-- description: Orion 1.0 wraps every admin response in data, changes several status codes, and makes the portable data dialect reject what it used to ignore. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# API and response shape

Break 10 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

What a call answers, and what a call is now refused for. Anything that parses Orion's responses or writes its JSON belongs here.

### Every admin response is now wrapped in `data`

**What changed.** Three response envelopes used to coexist on the admin plane. They were `{"data": …}`, the paginated `{data, total, limit, offset}`, and — from ten handlers — the fields bare at the top level. Now there is one. Every admin 2xx body puts its payload under `data`; List endpoints add the three pagination counters alongside it and nothing else. The one exception is the trace list, whose deviation is described immediately below.

**How you'll notice.** Ten endpoints return a body one level deeper than before:

| Endpoint | Was | Now |
|---|---|---|
| `GET /admin/engine/status` | `{version, uptime_seconds, …}` | `{"data": {…}}` |
| `POST /admin/engine/reload` | `{reloaded, workflows_count}` | `{"data": {…}}` |
| `GET /admin/connectors/circuit-breakers` | `{enabled, breakers}` | `{"data": {…}}` |
| `POST /admin/connectors/circuit-breakers/{key}` | `{reset, key}` | `{"data": {…}}` |
| `POST /admin/trace-dlq/purge` | `{purged, older_than_hours}` | `{"data": {…}}` |
| `POST /admin/workflows/{id}/test` | `{matched, trace, output, errors}` | `{"data": {…}}` |
| `POST /admin/workflows/validate` | `{valid, errors, warnings}` | `{"data": {…}}` |
| `POST /admin/{workflows,channels,connectors}/import` | `{imported, failed, errors}` | `{"data": {…}}` |
| `GET /admin/traces/{id}` | bare trace object | `{"data": {…}}` |

Everything already returning `{"data": …}` — all CRUD reads and writes, every list endpoint, `GET /admin/functions`, `POST`/`GET /admin/backups` — is byte-identical. Only the ten rows above changed *shape*.

**One exception: `GET /admin/traces`.** Its envelope is unchanged, but its
*fields* are not: `total` is now conditional and `next_cursor` is new. See
[the next section](#the-trace-list-no-longer-returns-total-by-default).

**What to do.** Add `.data` to the affected call sites:

```bash
# before
curl -s localhost:8080/api/v1/admin/engine/status | jq '.workflows_count'
# after
curl -s localhost:8080/api/v1/admin/engine/status | jq '.data.workflows_count'
```

Error bodies are unaffected — they stay `{"error": {code, message}}`, so `.data` present and `.error` present remain mutually exclusive. The data plane is unaffected too: `POST /api/v1/data/…` still answers `{"status": "ok", "data": …}` as before.

**Upgrade your clients in the same window.** This break is on the admin plane,
which is what the CLI and the console talk to. A client that predates 1.0 reads the old shape and will show empty or missing values against a 1.0 server rather than failing loudly.

| Client | What speaks 1.0 |
|---|---|
| `orion-cli` | **1.0.0 or newer.** It is versioned in lockstep with the server and ships in the same release, so matching the server's version is always right. A `0.2.x` CLI predates this envelope. |
| Orion Console (`orion-ui`) | The image built for this release. Pin a tag rather than running `:latest` across an upgrade, so the console and the server move together deliberately. |
| Your own tooling | Anything reading the ten endpoints above. If it is Rust, the [`orion-api`](https://crates.io/crates/orion-api) crate carries the exact response types and tolerates skew in both directions. |

Upgrade the server first, then the clients: a 1.0 CLI reads a pre-1.0 server's bare responses too (it accepts both shapes). That order has no window where nothing works.

The `orion-server dry-run` CLI subcommand prints the **unwrapped** shape (`{matched, trace, output, errors}`). It writes JSON to stdout for `jq`, not an HTTP response, so it gains nothing from an envelope.

### The trace list no longer returns `total` by default

**What changed.** `GET /api/v1/admin/traces` used to answer
`{data, total, limit, offset}` on every page. `total` is now **omitted** unless the request asks for it with `?include_total=true`. Two fields are new: `next_cursor` on the response, and `cursor` on the request. This is the one list endpoint that deviates from the shared pagination contract; every other one still returns `total` unconditionally.

**Why.** `total` was a `COUNT(*)` over the whole filtered set, recomputed on *every* page of the largest table Orion writes to. That is a full scan on PostgreSQL and InnoDB. Most callers page through the list and never read the number. Deep `offset` paging has the same shape of problem: the database counts past every skipped row. Keyset (`cursor`) paging skips nothing and counts past nothing, so page 500 costs what page 1 costs.

**How you'll notice.** Anything doing `.total` on the trace list gets `null`
(jq) or a missing-key error (typed clients). Nothing errors, and nothing else moved: `data`, `limit` and `offset` are exactly where they were.

**What to do.** Add `include_total=true` where you genuinely need the count,
or — better for anything that walks the list — switch to the cursor:

```bash
# before
curl -s "http://orion:8080/api/v1/admin/traces?limit=100" | jq '.total'

# after: ask for the count explicitly
curl -s "http://orion:8080/api/v1/admin/traces?limit=100&include_total=true" | jq '.total'

# after: page without a count and without an OFFSET
page=$(curl -s "http://orion:8080/api/v1/admin/traces?limit=100")
cursor=$(jq -r '.next_cursor // empty' <<<"$page")
curl -s "http://orion:8080/api/v1/admin/traces?limit=100&cursor=$cursor"
```

`next_cursor` is present only while a further page may exist — its absence is how you know you have reached the end. Treat the value as **opaque**; its encoding is not part of the API contract.

**Three request combinations are now `400` rather than silently wrong:**

| Request | Why |
|---|---|
| `?cursor=…&offset=10` | Two different paging modes; pass one |
| `?cursor=…&sort_by=updated_at` (or `status`, `channel`, `mode`) | `updated_at` is rewritten in place by every status change, so a cursor over it would skip rows. Keyset paging is offered only for the default `created_at` ordering |
| a `cursor` value you did not get from a `next_cursor` | Malformed cursor |

`?offset=` still works exactly as before for every sort column, including `updated_at`; nothing forces you onto the cursor.

If you embed Orion as a library, `TraceRepository::list_paginated` now returns `TracePage` (with `total: Option<i64>` and `next_cursor`) rather than `PaginatedResult<Trace>`. The other six repositories are unchanged.

### Bulk import reports dry runs in the same fields as real runs

**What changed.** `POST /admin/{workflows,channels,connectors}/import?dry_run=true`
used to return six fields for two facts: `would_create` and `would_fail` alongside a hardcoded `imported: 0` and a `failed` that always equalled `would_fail`. Both modes now return the same four fields.

```jsonc
// before, ?dry_run=true
{ "dry_run": true, "would_create": 12, "would_fail": 1, "imported": 0, "failed": 1, "errors": [...] }
// after, ?dry_run=true          (and wrapped in `data`, per the section above)
{ "data": { "dry_run": true, "imported": 12, "failed": 1, "errors": [...] } }
// after, real run
{ "data": { "dry_run": false, "imported": 12, "failed": 1, "errors": [...] } }
```

**What to do.** Read `imported`/`failed` in both modes and branch on `dry_run`.
The one trap: in a dry run `imported` is now the count that *would* be created, where it used to be a constant `0`. Any check of the form `if imported == 0` as a proxy for "this was a dry run" will now be wrong — test `dry_run` instead.

**Not changed:** all three imports still return **200** even when every item
failed, so check `failed` rather than the status code.

**Added in 1.0, additive:** both modes also carry `unchanged`, `skipped`
and a per-item `results` array, populated by the new `?on_conflict=skip` / `?on_conflict=new_version` upsert modes. See [Admin API › Export & Promotion](../../reference/admin-api/export-and-promotion.md#promoting-over-an-existing-estate-on_conflict). The default `on_conflict=fail` behaves exactly as before.

Four things are also additive in 1.0. A real (non-dry-run) import writes one audit row per entity written, alongside the `"{n} imported"` summary row. Channels and connectors gained `tags` and `?tag=` filtering, matching workflows. Status and rollout changes accept `?reload=defer` to batch engine rebuilds. An `X-Orion-Change-Context` request header is recorded in audit `details`. The 1.0 API also adds `GET /workflows/{id}/dependencies`, `content_hash` on every entity response, exports that read as one consistent snapshot. The `orion-server package` CLI that composes all of it. See [Admin API › Export & Promotion](../../reference/admin-api/export-and-promotion.md).

### `db_read` returns values for float and blob columns

**What changed.** `float4` / `REAL` / `FLOAT` columns and blob columns silently
returned `null`. They now return values, and a column that genuinely cannot be decoded raises an error instead of nulling. A `null` in the result now means only "SQL NULL".

- `Real` → JSON number.
- `Blob` (`bytea`, SQLite `BLOB`, MySQL `TEXT`/`JSON`) → JSON **string**: the
  UTF-8 text when the bytes are valid UTF-8, otherwise **lowercase hex** with
  no `0x` prefix (not base64).

New errors, all prefixed `db_read:`:

```
db_read: column 'x' is unreadable: <sqlx error>
db_read: column 'x' (Real) failed to decode: <sqlx error>
db_read: column 'x' holds NaN, which JSON cannot represent
```

**How you'll notice.** Workflows that used a `null` check to skip a float or blob column now see real data. A query touching an undecodable column now fails where it previously returned a row of nulls.

**What to do.** Review JSONLogic that treats these columns as always-null.
**Scope note:** this affects `db_read`, `data_query` (including nested
`include` queries), and `data_write`'s `RETURNING` path. It does **not** affect `db_write`, which returns `{"rows_affected", "last_insert_id"}`.

> Postgres `timestamptz`, `uuid`, `jsonb`, `numeric`, arrays, and enums were
> **never** silently nulled — sqlx rejects them while building the row, so the
> query already failed loudly. That behaviour is unchanged.

### `BAD_REQUEST` is now `VALIDATION_ERROR`, and an oversized result is a 500

**What changed.** Two 400 codes existed for one condition. Validators mixed `BAD_REQUEST` and `VALIDATION_ERROR` freely. Which one a refusal answered with was an accident of the internal variant the code path constructed, not a distinction a client could rely on. They are merged: every 400 that answered `{"code": "BAD_REQUEST"}` now answers `{"code": "VALIDATION_ERROR"}`. The message is unchanged, the status is unchanged, and `details[]` appears on a few more of them. Connector create and update refusals now name the offending field, the way channel and workflow refusals already did.

Separately, `RESPONSE_TOO_LARGE` — a workflow result exceeding `trace_queue.max_result_size_bytes` — moves from `502 Bad Gateway` to `500 Internal Server Error`. No upstream is involved in that condition, so 502 was the wrong claim; the code string is unchanged.

The 504 got the same one-condition-two-codes merge. An engine timeout answered `{"code": "TIMEOUT_ERROR"}` while the channel-level timeout guard answered `{"code": "TIMEOUT"}`, and which one a caller saw depended on the layer that fired first. Every 504 now answers `TIMEOUT`. The status and message are unchanged.

**How you'll notice.** A client branching on `error.code == "BAD_REQUEST"`
stops matching; branch on `VALIDATION_ERROR` (or on the 400 status). Anything alerting on a 502 from the data plane should alert on the `RESPONSE_TOO_LARGE` code instead. Retry/backoff or paging rules matching `TIMEOUT_ERROR` on a 504 silently stop firing; match `TIMEOUT` (or the 504 status).

**What to do.** Update literal matches on `BAD_REQUEST` and `TIMEOUT_ERROR`.
If you branch only on HTTP status, the 400 and 504 cases are untouched and the oversized-result case moves from 502 to 500.

### Updating an entity with no draft is a 404, not a 400

**What changed.** `PUT /api/v1/admin/workflows/{id}` answered `400` with *"No draft version found"* when the entity had no draft, as did the channel equivalent and `PATCH …/status` activation. Every other missing-row lookup in the admin API answers `404`. Which status a missing thing produced depended on which lifecycle method you reached first. All no-draft misses now answer `404 NOT_FOUND` with the same message.

Alongside it, the admin list surfaces were normalised: connector listings accept `sort_by` (`name` default, `connector_type`, `created_at`, `updated_at`) and `sort_order`. Previously they were hard-wired to `name ASC`, which remains the default. The version-history, trace, DLQ and audit-log query parameters are now declared in the OpenAPI document instead of only in prose.

**How you'll notice.** Automation that treated a 400 from an update as "no
draft — create one first" sees a 404 for that case now. The 400 still exists for genuinely invalid input.

**What to do.** Branch on 404 for the no-draft case. If you branched on the
message text, it is unchanged.

### Duplicate creates now return 409

`POST /api/v1/admin/workflows` and `POST /api/v1/admin/channels` with an id that already exists now return `409 Conflict` with `{"error": {"code": "CONFLICT", "message": "…"}}`. Through 0.3.x these returned `500 INTERNAL_ERROR`. Clients or retry logic that treated the 500 as transient should treat the 409 as a permanent client error. Pick a different id, or use the import endpoints, which report conflicts per item without failing the batch.

### A closed trace queue answers 503, not 500

**What changed.** `TraceQueue::submit` had two adjacent failure arms for one
condition — the queue cannot take this message. Queue *full* answered `503 SERVICE_UNAVAILABLE`; queue *closed* answered `500 QUEUE_ERROR`, which `is_retryable()` simultaneously reported as retryable. A retryable 500 is a contradiction, and the OpenAPI document had never described it: it lists queue-full and queue-closed together under `503`.

Both now answer `503` with code `SERVICE_UNAVAILABLE`. The `QUEUE_ERROR` code is gone.

**How you'll notice.** Only during shutdown, which is the one time the queue is
closed while requests still arrive. A client retrying on 503 now retries this too, which is the correct behaviour and was already what the documentation promised.

**What to do.** Nothing, unless you match on the literal string `QUEUE_ERROR`.

### Queue-full now returns 503 instead of hanging

**What changed.** When the async trace queue was full, submission blocked
waiting for capacity — an unbounded hang under load. It now sheds immediately.

**How you'll notice.** `POST /api/v1/data/{channel}/async` (and the REST-routed
`…/async` equivalents) returns **`503`** with code **`SERVICE_UNAVAILABLE`** —
*not* `QUEUE_FULL`. Disambiguate from other 503 answers by the message
(`Trace queue is full (N messages pending)` or `Trace queue memory limit exceeded …`) or, better, by `orion_trace_queue_rejected_total{reason="full"|"memory"}`.

**What to do.** Make async clients retry on `503`. Size the queue with
`trace_queue.buffer_size` (default `1000`) and `trace_queue.max_queue_memory_bytes` (default `104857600`, 100 MB). Sync requests never touch this queue.

### Open circuit breakers return 503 `CIRCUIT_OPEN`

**What changed.** A request rejected by an open circuit breaker used to surface
as `500` with code `ENGINE_ERROR`, indistinguishable from a genuine engine fault. It is now `503` with code `CIRCUIT_OPEN`.

**How you'll notice.** Look at `$.error.code` in the top-level error envelope:

```json
{"error": {"code": "CIRCUIT_OPEN",
           "message": "Circuit breaker open for connector 'orders-db' on channel 'orders'",
           "request_id": "..."}}
```

There is **no `Retry-After` header** on this response.

**What to do.** Update any client or alert that matched on `500` /
`ENGINE_ERROR` for breaker rejections, and treat `503 CIRCUIT_OPEN` as retryable.

> **Blind spot worth knowing.** This only holds when the failing task has
> `continue_on_error: false` (the default). With `continue_on_error: true` the
> request returns **HTTP 200** and there is no error envelope at all, so
> `$.error.code` is absent and an alert watching the status code reads a shed
> request as a success. Alert on
> `orion_circuit_breaker_rejections_total{connector, channel}`, not on the status
> code, if your workflows use `continue_on_error: true`.
>
> **This narrowed in 1.1.0.** Through 1.0 the accompanying `errors[]` entry was
> a sanitised `TASK_ERROR` naming nothing, leaving the rejection visible only in
> the metric and the persisted trace. Since 1.1.0 the breaker's own service kind
> survives classification, so the entry reads `"code": "circuit_open"` —
> lower-case, verbatim, and stays distinct from the `IO_ERROR` of a genuine
> connection failure and the `TIMEOUT_ERROR` of a slow one. A workflow can
> branch on it through `metadata._orion_errors.0.code`. The HTTP status is still
> `200`, so the metric remains the right alerting signal.

### The data dialect rejects what it used to ignore

**What changed.** `data_query`/`data_write` no longer approximate silently.
Ten changes can turn a previously "working" workflow into an explicit error or a differently ordered page. In every case the old behaviour was silently returning wrong or incomplete data. The first fires unconditionally.

- **A task with no `schema` now reaches nothing.** `unmapped` defaulted to
  `identity` — every name passing straight through to the physical one, so a
  dialect task without a `schema` reached every table the connector's database
  user could see, read *and* write. The default is now `reject`. *How you'll
  notice:* **every** `data_query`/`data_write` that declares no `schema` fails
  at its first request. Workflows already stored keep loading and activating;
  nothing fails at startup, so this surfaces on live traffic. The error reads
  `entity '<name>' is not declared in the task's schema: add "schema": … or add
  "unmapped": "identity" …`, and it is the one you get whatever else the query
  mentions, because the entity resolves before the filter, projection and sort.
  *What to do:* one of two things per task —

  ```json
  "schema": { "entities": { "orders": { "columns": { "id": {}, "total": {} } } } }
  ```

  declaring the entities and columns that task uses (the allowlist, and what
  you want long-term), **or** the one-line pass-through that restores 0.x
  behaviour exactly:

  ```json
  "schema": { "unmapped": "identity" }
  ```

  *Find them first:* this is the one change on the page that fails on live traffic rather than at startup. Do not discover it from your error rate. `orion-server preflight` names every stored task in this shape, workflow and task id. The direct query, if you would rather look yourself:

  ```sql
  SELECT name FROM current_workflows
  WHERE tasks_json LIKE '%data_query%' OR tasks_json LIKE '%data_write%';
  ```

  That one over-reports — it cannot see which of those tasks already declares a
  `schema`, which is exactly the part `preflight` does per task.

  Declare every column the task names in `fields`, `sort`, `filter`, `values`, `set`, `returning` and `include.<relation>.fields`. The last of those resolve against the *related* entity, so declare its columns too. A bare `{}` is a valid
  declaration when you want no rename or type hint.

  Three things to know once you declare columns. A read that names no `fields` now returns exactly the declared **queryable** columns rather than `SELECT *`. Setting `queryable: false` therefore hides a column from a field-less read too. An entity declaring *no* columns still reads all of them, and one whose every declared column is non-queryable is refused rather than widened back. A relation's `to` target needs no declaration for the relation itself to resolve. It does need one as soon as you name a column on it. An `include` must now name a `sort` key, which is such a column, so **an `include` over an undeclared entity cannot plan at all**. And a connector
  owner can refuse the `identity` escape hatch outright with
  `dialect.require_schema`, and bound physical names with
  `dialect.allowed_entities`. See the
  [dialect reference](../../reference/data-dialect.md#schema-guards).

- **Unknown envelope keys are rejected.** Stray or misspelled keys in the
  `query` envelope, the `write` envelope, an `include` selection, `on_conflict`
  or the inline `schema` (at any level) now fail. They used to be ignored:
  `"fileds"` selected every column, `"lmit": 5000` fell back to the default
  100, and a misspelled `filter` key made a delete unfiltered. *How you'll
  notice:* a task fails with `unknown key '…' in query envelope` (or `write
  envelope`, `include.<relation>`, `on_conflict`, or an unknown-field error
  from the schema). *What to do:* fix the key — the error names it. The pre-1.0
  flat `data_write` form is still accepted.
- **If you copied the old schema example, it never did what you thought.**
  `"table": "app_users"` was silently dropped — no rename, and identity mode
  where you believed you had an allowlist. The field is `physical`, and
  `"type": "string"` should be `"text"`. The strict schema surfaces this as an
  error instead of silently under-protecting.
- **`include` and many-to-many filters error on MongoDB and Elasticsearch.**
  They used to return parents with silently empty children, or wrong rows; both
  now raise `FeatureUnsupportedByTarget`. *What to do:* on a doc store, fetch
  the related documents with a second query, or model them embedded/nested and
  filter with `some`.
- **Mongo projections no longer include `_id` unless you project it.**
  `fields: ["name"]` now returns `{name}` on every backend. Project the id
  explicitly if you relied on it.
- **`skip` is capped at `query.max_skip` (default `10000`) on every backend.**
  A deeper offset is rejected, never clamped — SQL and MongoDB previously
  accepted any depth. Raise `query.max_skip` (or `ORION_QUERY__MAX_SKIP`) if
  you genuinely page deeper.
- **`include` now requires a `sort`, and its page is per parent.** An `include`
  selection without an order key is rejected: the per-parent page is cut inside
  the database (`ROW_NUMBER() OVER (PARTITION BY <fk> ORDER BY <sort>)`), so
  "the first 5 orders" has no defined answer without one — it used to be
  whichever rows the plan emitted, and a different set on the next run.
  **This fails at request time, not at activation:** the dialect envelope is
  not validated when a workflow is activated, so a stored workflow using
  `include` without a `sort` keeps activating and starts failing on live
  traffic. *How you'll notice:* a task fails with `include.<relation> requires
  a 'sort' — the per-parent page needs a deterministic order key`. *What to
  do:* grep your workflows for `"include"` before upgrading and add a `sort` to
  each selection (`"sort": [{"id": "asc"}]` is stable and unsurprising). The
  `sort` may name a column your `fields` does not — it is used for ordering
  only and does not appear in the nested objects. The window function is
  supported by every SQL backend Orion renders for (SQLite ≥ 3.25,
  PostgreSQL ≥ 8.4, MySQL ≥ 8.0), so a MySQL 5.7 server cannot run an
  `include`.

  On MongoDB and Elasticsearch nothing changes. `include` was already rejected there with `FeatureUnsupportedByTarget`, and it still is. The sort requirement is the SQL planner's, so a doc-store caller still gets the capability error saying `include` is SQL-only.
- **`include.limit` is bounded by `query.default_limit` / `query.max_limit`,
  per parent.** An `include` with no `limit` used to fetch *every* child of
  every parent on the page and truncate in memory; it now fetches
  `default_limit` (100) children **for each parent row**. A `limit` above
  `query.max_limit` (1000) is rejected with a limit-exceeded error, never
  clamped — the same rule the envelope's own `limit` has always had. *How
  you'll notice:* a task fails with `requested limit N exceeds the configured
  maximum M`, or a nested array that used to be complete now stops at 100
  entries. *What to do:* set an explicit `include.limit`, or raise
  `query.max_limit` (`ORION_QUERY__MAX_LIMIT`). The page is now bounded at
  `parents × include.limit` rows overall, which is the point.
- **Null ordering is inverted on SQL and Elasticsearch: a null sorts as the
  *smallest* value.** Nulls come first on `asc` and last on `desc`. SQL
  emulated "nulls last on `asc`" (with an `IS NULL` prefix sort key on MySQL)
  and Elasticsearch set `"missing": "_last"`, while MongoDB's `find` cannot
  express that rule at all, so the same envelope paged differently on Mongo,
  silently, against a documented promise of deterministic ordering. The shared
  rule is now the one every backend states natively, so the other four move to
  meet Mongo. *How you'll notice:* nothing errors — pages sorted on a nullable
  column come back in a different order, and `skip`-based paging over such a
  column visits rows in a different sequence. *What to do:* if the position of
  nulls matters, filter them out (`{"!=": [{"field": "col"}, null]}`) or sort
  on a non-nullable column first.
- **MongoDB no longer maps `id` to `_id` for you.** Any physical name equal to
  `id` — in filters, projections, sorts, inserted documents, `set` clauses and
  `on_conflict` targets — used to be rewritten to `_id`, so a schema
  deliberately mapping a key onto `id` meant `_id`, and a collection with a
  genuine non-key `id` field was unqueryable. Elasticsearch documented the
  opposite rule two files away; both document stores now pass names through
  exactly as the schema resolved them. *How you'll notice:* a Mongo filter or
  projection on `id` matches nothing where it used to hit the document key —
  documents written before the upgrade carry theirs in `_id`. *What to do:*
  declare the rename, which is what Elasticsearch already required and is also
  what makes inserts carry the id and upsert-on-`id` legal:

  ```jsonc
  "schema": { "entities": { "users": { "columns": { "id": { "name": "_id" } } } } }
  ```

  Without it, `id` is an ordinary field on every backend. If your collection
  genuinely has an ordinary `id` field beside `_id`, do nothing — it is
  queryable now, which it was not before.
- **Every `data_write` result carries a `status`, and a partial bulk is no
  longer an error.** Results gained `"status": "ok"`, so anything asserting on
  the exact result object (`{"rows_affected": 1}`) sees one extra key. A bulk
  `insert` means three different things underneath:

  | Backend | Model | On failure |
  |---|---|---|
  | SQL | **Atomic** | Every row or none — now in an explicit transaction rather than by accident of the renderer's shape |
  | MongoDB | **Prefix-applied** | `insert_many` is ordered: it stops at the first rejected document, commits everything before it, and never attempts the rest |
  | Elasticsearch | **Arbitrary-applied** | `_bulk` attempts every action independently, so any subset can land |

  All three used to return one row count or one opaque error, so documents had
  been written and the caller could not tell which. On MongoDB and Elasticsearch a bulk that applied *some* of its rows now returns `"status": "partial"`, with a per-item array indexed by your `values` array. The task reports audit status **207** instead of failing:

  ```json
  {
    "status": "partial",
    "inserted": 2, "failed": 1, "skipped": 2,
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

  `failed`, `skipped` and `items` appear only when there is something to report, so a clean bulk keeps the shape it had plus `status`. The `skipped` status means the backend never attempted the item, which only ordered MongoDB produces.

*What to do:* a workflow that previously relied on the task erroring to halt the pipeline now continues. Branch on `status` and compensate, using `items` to name the indices to retry or roll back. A bulk where *nothing*
  landed is still a hard error, and SQL connectors are unaffected.

### Unknown keys in a channel config are now refused

**What changed.** `ChannelConfig` rejects keys it does not recognize. Before
1.0 they were silently ignored. The refusal applies at every nesting level. A typo *inside* a guard's body fails the same way: `rate_limit`, `cache`, `deduplication`, `tracing`, `backpressure`, `auth` and `response` all count. Such a typo previously fell back to that field's default. A misspelled `rate_limit.key_logic` silently meant per-client-IP keying, and a misspelled `deduplication.window_secs` silently took the default window.

**Why.** Every key in a channel config is a *guard*. A key Orion does not recognize is a guard that never runs. Nothing re-serialises `config_json` — the stored document is the one you wrote — so the mistake survives every reload. A stored `"deduplicaton"` meant no idempotency, no error, forever. The config file, the connector configs and both dialect envelopes already rejected unknown keys; channel config was the last surface that did not.

**How you'll notice.** A channel whose stored config carries a stray key is quarantined at load. It is refused at every ingress, and listed on `/health` and the admin surface with the reason. On create and update it is a `400` naming the key. This is also the mechanism behind the `cors`, `max_concurrent` and `queue_depth` entries elsewhere on this page: all four are the same failure.

**What to do.** Run `orion-server preflight` — it names every stored channel
with an unparseable config and, for the two renames, the key to use instead.

### Channel names must be unique

**What changed.** A channel name may belong to only one `channel_id`.
Creating, updating, or importing a channel whose name another channel's current version already holds answers **409**, and activation refuses a name another *active* channel holds. Before 1.0 the collision stored cleanly and was resolved silently at runtime: the data plane and `channel_call` address channels by **name**. One of the two won the registry slot and the other's requests ran the winner's workflow.

**What to do.** Run `orion-server preflight` before upgrading — it reports
every name held by more than one `channel_id` (`channel-names` check). Rename all but one (new version with a distinct name, activate it) or delete the redundant channels. An estate without duplicates — any estate that worked predictably — is unaffected.

### Channel activation now requires an active workflow

**What changed.** `PATCH /admin/channels/{id}/status` with `{"status": "active"}` now answers **400** in three cases. The channel's `workflow_id` is unset, names a workflow that does not exist, or names one with no active version. It used to succeed and quarantine the channel at the next engine load — the same outcome, discovered later, with no error to the caller. The docs and the `/validate` warning always claimed this gate existed; now it does.

**What to do.** Activate in dependency order: connectors → workflows → channels. Any working deployment script already did that, since an out-of-order channel never served. A script that relied on activate-then-fix ordering must activate the workflow first. `?dry_run=true` on the same endpoint pre-flights the gate without writing.

### `route_pattern`, `topic` and `consumer_group` are capped at 255 characters

**What changed.** Create, update and import reject values longer than 255
characters in these three fields (field error code `TOO_LONG`). Before 1.0 there was no length check at all.

**Why.** MySQL stores all three columns as `varchar(255)`; SQLite and
Postgres use unbounded `text`. A longer value stored fine on two backends and failed on the third — a silent divergence the portable schema exists to prevent. The narrowest backend sets the limit (characters, not bytes).

**How you'll notice.** Only if you write a value that long: a `400` naming
the field. Stored rows are not re-checked at load. A pre-1.0 row over the limit (only possible on SQLite/Postgres) keeps serving, but its next edit must shorten the value.

### A REST route matches byte-exactly, and decodes parameters once

**What changed.** Three visible changes on `/api/v1/data/*`:

- **Case matters now.** `/ORDERS/1` no longer matches a channel declaring
  `/orders/{id}` — it answers 404. Fix client URLs, or register the alternate casing
  as its own route (two casings are two distinct routes now, so both can be
  active).
- **`metadata.params` arrive percent-decoded exactly once.** `/orders/a%2Fb`
  now matches with `id == "a/b"`; previously `%2F` was decoded *before*
  matching and acted as a path separator, so the request never matched at all.
  If a workflow hand-decoded a param, remove that step — decoding twice changes
  meaning (`a%252Fb` arrives as `a%2Fb`, not `a/b`).
- **Malformed escapes are refused.** A path carrying an invalid
  percent-sequence (`%ZZ`, a truncated `%2`) is answered with `400` instead of
  being matched literally.

Percent-encoding an unreserved character is still equivalence per RFC 3986: `/%6Frders/1` matches `/orders/{id}`.

**Also a validation-time break:** a `route_pattern` containing `%` is now
rejected on create, update and import. Patterns are written literally and requests match by their decoded value. An escape in a pattern was only ever reachable through a double-encoded request — write the literal character instead. Already-active channels keep their existing behaviour until you next edit them.

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
