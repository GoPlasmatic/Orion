# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- **Admin credential guessing is now metered and throttled.** The middleware
  stack was registered so that admin auth ran *outside* rate limiting; since it
  returns 401 without invoking the inner service, a wrong key never reached the
  limiter. The layer order is corrected, and failed admin authentication now
  applies a per-client exponential backoff (5 free attempts, then 500 ms
  doubling to a 30 s cap, cleared on success). Failures are counted by the new
  `admin_auth_failures_total{reason}` metric instead of the shared
  `errors_total{type="auth_failure"}`.
- **`fields` and `sort` no longer bypass the schema entirely.** `resolve_field`
  had exactly one call site — the filter lowerer — so the projection and sort
  keys reached SQL, MongoDB and Elasticsearch as **raw logical strings**. Three
  consequences, all silent:

  - With `{"secret": {"queryable": false}}`, `fields: ["secret"]` still emitted
    `SELECT "secret"` and returned the value. The allowlist the dialect
    documents protected the filter and nothing else. **A schema relying on
    `queryable: false` was not hiding the column from a projection.**
  - `sort` could order by a column the caller may not read.
  - A column rename applied to the filter and not to the projection, so
    `fields: ["email"]` against `{"email": {"name": "email_addr"}}` selected a
    quoted *literal* rather than the renamed column.

  The whole envelope — `fields`, `sort` and `include.fields` — is now resolved
  before any backend sees the spec, so no renderer can receive a logical name.
  Relation join keys resolve too (renames and identifier rules, deliberately
  not the caller-facing allowlist: they are operator-declared structure, not
  caller input), which fixes include grouping against a renamed key column.

- **`returning` no longer bypasses the read allowlist.** `data_write`'s
  `returning` resolved through a helper that fell through to the raw column
  name regardless of policy, so `{"op": "insert", …, "returning": ["secret"]}`
  read back **any column the database user could see** — including one the
  schema declared `queryable: false`, and any column at all under
  `unmapped: "reject"`. The helper's doc comment justified skipping the
  *`writable`* check and silently skipped the allowlist with it. It now
  resolves through the same path as `filter`, gated on `queryable` (reading
  back a non-writable column stays legitimate). **A schema that relied on
  `queryable: false` to hide a column was not hiding it.**

- **Identifier validation is now one rule across the read and write paths.**
  The read path rejected empty and dotted names; the write path checked
  nothing; **neither rejected a leading `$`**. Three silent consequences:
  `{"field": "$where"}` in identity mode reached MongoDB as a raw document key
  (where `$`-prefixed keys are operators); `values: {"a.b": 1}` wrote a nested
  path on MongoDB but a literal column named `a.b` on SQL — one envelope, two
  meanings; and `values: {"": 1}` emitted `INSERT INTO "users" ("")`. A shared
  `validate_identifier` now runs wherever a logical name becomes a physical
  one, including rename targets and `physical` table names, and also rejects
  quote, escape and control characters as defence in depth around F25.

- **A filter matching every row no longer bypasses the `data_write` safety
  guard.** `{"op":"delete","target":"t","filter":{"and":[]}}` and other
  tautological filters skipped both the `"all": true` acknowledgement and
  `write.allow_unfiltered`, deleting every row. The guard now derives from the
  lowered condition rather than the presence of a `filter` key.

- **A many-to-many junction reached the SQL renderer unvalidated.**
  `resolve_relation` copied a junction's `table`, `local` and `foreign` names
  into the renderer without `validate_identifier` — the one identifier channel
  that skipped the boundary rule, so a schema carrying quote characters in a
  junction name reached `Alias::new` raw. The gap is closed, and identifier
  safety no longer rests untested on a transitive dependency: two property
  tests now fuzz every identifier channel of the write path (insert `target`
  and inserted column, update `set` column and `returning` name) and the read
  path (`source`, `fields`, `sort`, filter fields) with quotes, backslashes and
  unicode, asserting boundary rejection or safe quoting on all three SQL
  dialects (F25).

- **The SSRF validator never looked at the URL scheme.**
  `validate_url_not_private` parsed the URL and vetted every resolved address,
  so `gopher://public.example:70/` passed, and the `unwrap_or(80)` port default
  could pin the wrong `SocketAddr` set for a non-http scheme. Anything outside
  `http`/`https` is now rejected before any host or DNS work — *"only http and
  https are allowed"*. Every caller is an HTTP egress path, so nothing
  legitimate is lost (S7).

- **Connector secrets in URL query strings round-tripped in the clear.** URL
  redaction covered userinfo only, so `?api_key=SECRET` and friends came back
  verbatim from `GET /api/v1/admin/connectors`. Query parameters whose name
  satisfies the same secret-key predicate as object keys are now masked
  (`?api_key=…`, `?sig=…`, `?X-Amz-Signature=…`), and the denylist gains
  `bearer`, `dsn`, `webhook` (substrings) plus `pat` and `sig` (exact matches).

  Because one string can now carry several maskable positions, the mask
  round-trip guard is positional: each masked position — the userinfo password,
  each secret-named query value — is restored independently from the stored
  value, so rotating one in-URL secret while sending the other back masked
  restores the masked one instead of persisting `******` as the live
  credential. A masked position with no stored counterpart — including a
  literal `******` query value under a non-secret parameter name, which masking
  can never produce — is refused with `400` on create and update.

  **Still shown in the clear:** a capability token embedded in a URL *path* (a
  Slack-style webhook) under a generic key, because a path segment carries no
  name to judge. Store it under a secret-looking key (`webhook_url`) and the
  key-name rule masks the whole value (S18).

- **`/docs` and `/api/v1/openapi.json` were served unconditionally, to
  anonymous callers, in production (breaking).** Both endpoints are
  unauthenticated and the spec publishes the complete admin API surface — route
  shapes, request schemas, the `admin_auth.header` semantics — so every
  production deployment advertised it. The new `server.docs.enabled`
  (`ORION_SERVER__DOCS__ENABLED`) gates them: unset serves them only when
  `environment` is not a production variant (the same prefix rule that turns
  the admin-auth and CORS-wildcard checks fatal), an explicit `true`/`false`
  always wins, and disabled means the routes are not registered at all — `404`,
  not `401`, so their existence is not advertised. Production tooling that
  reads the served spec should set `server.docs.enabled = true` or switch to
  `orion-server dump-openapi`, which works offline regardless (S17).

- **A workflow could poison its own channel's dedup store and response cache
  (breaking).** Every `backend: "memory"` cache connector, the built-in dedup
  store and the response cache shared one in-process instance, so a workflow
  `cache_write` with a crafted `dedup:{channel}:{key}` key manufactured a `409`
  for a real request, a forged `cache:{channel}:{hash}` entry was served as a
  cached response, two memory connectors silently shared one keyspace, and a
  hot workflow cache evicted dedup entries out of the single shared LRU budget.
  In-memory backends are now distinct instances per purpose (workflow / dedup /
  response cache) and connector name, each with its own
  `engine.max_memory_cache_entries` budget.

  That makes the setting a **per-namespace** bound rather than a shared one —
  worst-case resident entries are `max_memory_cache_entries` × (2 built-in
  stores + up to 3 namespaces per memory connector) — so a memory-constrained
  host sized against the old single bound should divide the setting by its
  namespace count. Memory state never survived a restart, so migration is a
  no-op. Redis backends are deliberately *not* partitioned: they are external,
  shared across nodes, and legitimately read keys other systems wrote — use
  separate Redis databases where you need isolation (S19, N11).

### Breaking

- **A REST channel's `route_pattern` and `methods` must now be well-formed.**
  They were checked for non-emptiness and nothing else, so
  `methods: ["POTS"]` and `route_pattern: "orders/{id"` were created,
  activated and reloaded — and then never matched a request, with nothing
  reporting it. The channel was simply dead.

  A pattern must start with `/`, have no empty segments, and write each
  parameter as a whole `{name}` segment with a valid identifier and no
  duplicates; a method must be one Orion can route (`GET`, `POST`, `PUT`,
  `PATCH`, `DELETE`, `HEAD`, `OPTIONS`). Checked on **update** as well as
  create, and every problem comes back in one response. Existing active
  channels are not re-validated; you meet this when you next edit one.

- **A channel cannot be activated onto a route another active channel already
  claims.** Two channels declaring `GET /orders/{id}` resolved by database row
  order: which one served could differ between nodes and change on any reload,
  and the loser's declared path silently ran the winner's workflow. The
  incumbent wins, so activating a channel can never take a running one down.
  Parameter names are not part of the match — `/orders/{id}` and
  `/orders/{order_id}` collide. Rows stored before this resolve
  deterministically now and log a warning naming both sides.

- **`POST /api/v1/data/{channel}/async` always returns a usable `trace_id`.**
  With `trace_storage.mode = "off"` it used to mint a throwaway UUID, answer
  202 with `{"trace_id": null, "trace_token": null}` plus a `Warning: 299`
  header, and enqueue the work anyway — a receipt whose documented follow-up
  (`GET /admin/traces/{id}`) was structurally impossible. Appending `/async`
  *is* the request for a result to be fetched later, so the trace row is
  written before the 202 and the worker persists the outcome. `off` still
  applies in full to the synchronous endpoint, where the caller already has
  the answer. `trace_id` and `trace_token` are now required in the schema and
  the `Warning` header is gone.

- **`_orion.profile` is now `version: 2`.** See below.

- **`POST /admin/workflows/import` reports per-item failures instead of
  aborting.** One malformed item used to abort the whole batch with a 400,
  while the identical mistake against `/channels/import` or
  `/connectors/import` produced one failed entry and imported the rest —
  three endpoints, one documented request shape, two behaviours. All three now
  share one driver.

- **`POST /admin/workflows/validate` field paths are the create-path ones.**
  `name` is now `workflow.name`, and so on: the endpoint runs
  `validate_create_workflow` itself rather than a parallel re-implementation,
  which is what makes `valid: true` mean "create would accept this". See
  Fixed.

- **Entity ids that collide with a static admin sub-resource are refused.**
  `import`, `export`, `validate`, `versions`, `status`, `rollout`, `test`,
  `circuit-breakers`, `purge`, `requeue` and `reload` cannot be used as a
  workflow, channel or connector id: those paths sit alongside `/{id}`, so an
  entity named `import` was unaddressable and `DELETE /admin/workflows/import`
  audit-logged a delete of nothing.

- **A bare JSON object is now accepted as the data-plane payload.** The
  endpoint had three behaviours for three body shapes and documented one:
  `{"data": …}` was the envelope, an empty body became `{"data":{}}`, and
  `{"amount": 5}` — the obvious thing to send — failed with *missing field
  `data`*. One rule now: an object carrying `data` or `metadata` is the
  envelope, anything else is the payload. Strictly widening; previously-400
  requests now succeed.

- **`retry` is gone from the `db` and `es` connector configs.** It was declared,
  validated and documented on both, and the only reader of a retry policy is
  `http_call` — so `{"type":"db", …, "retry":{"max_retries":5}}` did exactly
  nothing while the field table promised "retry with exponential backoff".

  It is not coming back as a working field: a database or `_bulk` call that
  timed out may already have been applied, so a blind re-send duplicates it —
  the same hazard that made `http_call` retry idempotent methods only. Bound
  those calls with `connect_timeout_ms` / `query_timeout_ms` /
  `request_timeout_ms`, and let the circuit breaker shed load from a dependency
  in trouble. **Stored connectors carrying the key still load**; it is ignored,
  as it always effectively was.

- **A workflow cannot activate against a connector of the wrong type.**
  Activation checked only that the referenced connector *existed*, so a task
  pointing `cache_read` at a `db` connector — or `publish_kafka` at anything
  that is not `kafka` — activated cleanly and then returned 500 on its first
  request. Each function now declares the connector types it can run against,
  and a mismatch is a 400 at `PATCH /admin/workflows/{id}/status` naming the
  function, the connector and what was required.

  The same check covers the one cross-field rule the static schema cannot
  express: `data_query` / `data_write` against a **MongoDB** connector must set
  `database`, because a Mongo connection string carries no default one. The
  field stays optional in the schema — the identical task shape is valid against
  SQL and Elasticsearch — and is required once the connector is known.

- **Renaming a connector is refused while an active workflow references it.**
  Workflows bind connectors by name, and nothing tied the two together: a rename
  left every referencing workflow resolving to nothing, which is a 500 per
  request with no error at rename time. Repoint or archive those workflows
  first. Pool eviction now covers both the old and the new name, so the old
  entry no longer holds TCP connections against the remote database's
  `max_connections` until the LRU happens to reclaim it.

- **`_orion.profile` is now `version: 2`.** `handlers[].nested` lists only the
  calls that actually ran inside that `channel_call`; v1 attached every nested
  sample to every top-level one, so a workflow fanning out to two channels
  reported each one's children under both. A call with no children now omits the
  key rather than emitting an empty array. Branch on `version`.

- **`data_write`'s mutation envelope is nested under `write`,** mirroring
  `data_query`'s `query`. It used to be flat: `op`, `target`, `values`, `set`,
  `filter`, `on_conflict`, `returning` and `all` sat alongside the handler's own
  `connector`, `schema`, `params`, `database` and `output`. So the two halves of
  one dialect read differently, the envelope could never grow a field named like
  any of those five, and there was no single JSON value that *was* the envelope
  for validation, logging or a builder UI.

  ```jsonc
  // before                                  // after
  { "connector": "db",                       { "connector": "db",
    "op": "update", "target": "users",         "params": { "id": {"var": "data.id"} },
    "set": { "status": "off" },                "output": "data.w",
    "params": { "id": {"var": "data.id"} },    "write": {
    "output": "data.w" }                         "op": "update", "target": "users",
                                                 "set": { "status": "off" } } }
  ```

  **The flat form is still accepted for one release**, so existing workflows
  keep running; `write` wins if a task carries both. Validation errors are now
  reported under `…function.input.write.<field>`, and a `data_write` with
  neither shape is rejected at create naming `write`.

- **Four config sections renamed, and audit-log retention split out of
  `[queue]`.** Each of these cost a paragraph of documentation to explain what
  the key actually did:

  | Pre-1.0 | 1.0 | Why |
  |---|---|---|
  | `[queue]` | `[trace_queue]` | It only ever configured the async *trace* queue |
  | `queue.trace_retention_hours` | `trace_queue.retention_hours` | Prefix redundant inside the renamed section |
  | `queue.trace_cleanup_interval_secs` | `trace_queue.cleanup_interval_secs` | Drove both cleanup jobs, named for one |
  | `queue.audit_retention_days` | `audit.retention_days` | Audit rows have nothing to do with the trace queue |
  | — | `audit.cleanup_interval_secs` (new, default `3600`) | Audit cleanup no longer borrows the trace job's cadence |
  | `[channels]` | `[channel_filter]` | It selects which channels to load; it does not configure channels |
  | `[tracing.storage]` | `[trace_storage]` | `[tracing]` is OTLP export; this is Orion's own trace rows — two unrelated concerns under one section |
  | `ORION_ENV` | `ORION_ENVIRONMENT` | The last name breaking the `ORION_` + field-path rule |

  Environment variables follow their keys: `ORION_QUEUE__*` → `ORION_TRACE_QUEUE__*`,
  `ORION_CHANNELS__*` → `ORION_CHANNEL_FILTER__*`, `ORION_TRACING__STORAGE__*` →
  `ORION_TRACE_STORAGE__*`.

  **A retired variable is a startup error, not a silent no-op.** Overrides are
  matched by name rather than deserialized, so `deny_unknown_fields` cannot see
  them — a renamed section would otherwise leave `ORION_QUEUE__WORKERS` set and
  quietly ignored. For `ORION_ENV` that would have been a security regression:
  falling back to `development` turns the production admin-auth and wildcard-CORS
  checks from startup errors back into warnings. Orion now refuses to boot and
  names every offender at once. Retired *file* keys are caught by
  `deny_unknown_fields`.

- **One response envelope across the admin plane.** Every admin 2xx body now
  carries its payload under a top-level `data` key; list endpoints add `total`,
  `limit` and `offset` alongside it and nothing else. Three envelopes used to
  coexist, and ten handlers returned their fields bare at the top level:

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

  `POST /admin/backups` and `GET /admin/functions` hand-rolled the `{"data": …}`
  wrapper and are unchanged on the wire; `GET /admin/traces` hand-rolled the
  pagination envelope and is likewise unchanged. All three now go through the
  shared helpers, so they cannot drift again.

- **Bulk import returns the same four fields whether or not it is a dry run.**
  `?dry_run=true` used to answer with six fields for two facts —
  `would_create` and `would_fail` next to a hardcoded `imported: 0` and a
  `failed` that always equalled `would_fail`. Both modes now return
  `{dry_run, imported, failed, errors}`; in a dry run `imported` is the count
  that *would* be created rather than a constant 0. Read `dry_run` to tell the
  modes apart.

  Unchanged: all three imports still return **200** even when every item
  failed. Callers must check `failed`, not the status code.

- **The trace read endpoints moved to the admin plane:**
  `GET /api/v1/data/traces` → `GET /api/v1/admin/traces`, and
  `GET /api/v1/data/traces/{id}` → `GET /api/v1/admin/traces/{id}`. **No
  redirect** — the old paths now resolve as channel names, so a stale client
  gets 404.

  The list was already admin-guarded, so its placement on the data plane was a
  naming lie — and a functional one: both were static routes, which axum
  resolves ahead of the `/{*path}` catch-all, so **a channel named `traces` was
  permanently unreachable** (`POST /api/v1/data/traces` returned 405) with no
  reserved-name check to explain it. The data plane is now a single catch-all
  and the rate limiter's `traces` special case is gone.

  Access rules are unchanged. `GET /api/v1/admin/traces/{id}` still accepts
  *either* an admin credential or the submission's `trace_token`, making it the
  one path under `/api/v1/admin` exempt from the blanket admin guard.
- **Seven connector config fields that were never read are removed:**
  `db.driver`, `db.auth`, `cache.default_ttl_secs`, `cache.max_connections`,
  `cache.auth`, `cache.retry`, and `kafka.group_id` (the connector one — the
  `[kafka] group_id` server setting is unaffected). Each was accepted,
  validated, persisted, returned by `GET /connectors`, and documented with a
  default. `db.driver` was the worst: it looked like the thing that selects the
  backend and is not, so `driver: "mysql"` with a `postgres://` URL connected
  to Postgres.

  **Stored connector configs keep loading** — connector configs do not use
  `deny_unknown_fields`, so a 0.3.x row carrying these keys deserializes fine
  and they are ignored, exactly as they always effectively were. Nothing to do
  on upgrade; delete them from your configs at leisure. Credentials go in
  `connection_string` / `url`; cache TTL is per-`cache_write` via `ttl_secs`.
- **The `storage` connector type is removed.** It was accepted, validated,
  persisted and listed by `GET /connectors` for the whole 0.x line with no
  handler behind it — `POST /connectors` returned 201 and every workflow
  referencing the connector failed at request time. The documentation
  advertised S3, GCS and local-filesystem support with a full field table for
  something that did not exist; that section is gone.

  `connector_type: "storage"` is now rejected at create. An existing stored row
  is reported as a connector load issue (`stage: "removed_type"`) naming the
  removal, visible on `/health` and `GET /api/v1/admin/connectors` — and fatal
  at boot when `engine.fail_on_connector_load_error = true`. **Delete or
  disable such connectors before upgrading.** Nothing that worked stops
  working: there was never a working configuration to preserve.
- **An unknown key in the config file is now a startup error.** Every config
  struct was `#[serde(default)]` with no unknown-field rejection, so
  `[server] wrokers = 4`, or a whole misspelled section, booted clean with
  defaults and no way to notice. All 24 structs now carry
  `deny_unknown_fields`, and the error names the offending key.

  This covers the **config file only**. A misspelled `ORION_*` environment
  variable is still ignored silently: overrides are read by name rather than
  deserialized, so there is nothing for serde to reject. Check spelling against
  `docs/src/configuration/reference.md`.
- **One output-field name across every function: `output`.** `http_call` and
  `channel_call` called their destination path `response_path` while the other
  eight handlers called it `output` — two names for one concept, and the
  most-touched field in the task JSON contract. Both handlers now take
  `output`. `response_path` is still accepted so 0.3.x workflows load
  unchanged; when a task carries both, `output` wins.

  | Function | Pre-1.0 | 1.0 | Default if omitted |
  |---|---|---|---|
  | `http_call` | `response_path` | `output` | response discarded |
  | `channel_call` | `response_path` | `output` | `"data"` |
  | the other eight | `output` | `output` | `"data"` |

  The differing defaults are deliberate and unchanged in this release.
- **Every metric is renamed with an `orion_` prefix** — `messages_total` is now
  `orion_messages_total`, and so on for all 33 families. The bare names were
  generic enough to collide in a shared registry (`errors_total`,
  `active_workflows`, `db_pool_size`). **Update dashboards and alert rules
  before upgrading.**
- **Histograms are now real Prometheus histograms.** Without configured buckets
  the exporter rendered all seven `*_seconds` families as *summaries with
  pre-computed quantiles*, which cannot be aggregated across replicas —
  directly at odds with cluster mode. Queries using
  `histogram_quantile()` over `_bucket` series now work; queries reading the
  old summary quantiles must be rewritten.
- **In cluster mode every metric carries an `instance` label** identifying the
  replica. Recording rules that aggregate without `by`/`without` may need
  updating.
- **Plaintext `admin_auth.api_keys` entries must be at least 32 characters.**
  Previously `api_keys = ["a"]` was a valid production credential. Shorter keys
  are a hard config error when `environment` starts with `prod`, and a warning
  otherwise. `sha256:` entries are exempt. Generate keys with
  `openssl rand -hex 32`.
- **`rate_limit.endpoints.admin_rps` now defaults to `20`** instead of being
  unset. Previously the admin plane fell back to `default_rps` (100) — the same
  budget as the anonymous data plane. Set it to `null` (or an empty string via
  the environment variable) to restore the fall-back.
- **401, 429 and recovered-panic 500 responses now carry security headers and
  `x-request-id`,** and the error envelope for them includes `request_id`.
  Clients asserting on the absence of these will see new headers and one new
  body field.
- **Browser preflight (`OPTIONS`) to `/api/v1/admin/*` is now answered by the
  CORS layer** rather than rejected with 401. Any client relying on preflight
  failing closed should note the admin API was previously unusable from a
  browser whenever `admin_auth.enabled = true`.
- **Both dialect envelopes and the inline `schema` are strict.** Unknown keys
  in the `data_query` envelope, the `data_write` envelope, `include` selections
  and `on_conflict` were silently ignored: `"fileds"` selected every column,
  `"lmit": 5000` fell back to the default 100, `"retuning"` returned nothing,
  and a misspelled `filter` key made a mutation unfiltered. They are now
  rejected with an error naming the offending key — fix the key it names. The
  pre-1.0 flat `data_write` form keeps working: the handler strips its own keys
  before the strict parse.

  Every `schema` struct rejects unknown keys too, which surfaces a trap the
  documentation's own example set. It used `"table"` where the field is
  `physical` — silently dropped, so authors got a wide-open identity-mode
  registry believing they had configured a rename — and `"type": "string"`,
  which is not a `FieldType`. The example is fixed, and a test parses it
  verbatim and asserts it means what the prose says. A stored schema carrying a
  stray key now fails loudly instead of silently not applying (W6, W5).
- **`include` and many-to-many filters raise a capability error on MongoDB and
  Elasticsearch.** `include` was parsed and silently dropped by both doc-store
  translators — the caller got parents with no children and no error — and a
  `some`/`all`/`none` over a `through` relation rendered as a plain
  `$elemMatch`/`nested` on the relation name, returning wrong rows. Both now
  raise `FeatureUnsupportedByTarget`, the same gate include planning already
  applied to m2m on SQL, and the parity table documents both rows. On a doc
  store, fetch the related documents with a second query, or model them
  embedded/nested and filter with `some` (F26, W11).
- **Mongo projections no longer leak `_id`.** `fields: ["name"]` returned
  `{name}` on SQL and Elasticsearch and `{_id, name}` on MongoDB — one
  envelope, two result shapes. `_id` is now suppressed unless explicitly
  projected; project it if you relied on it (W9).
- **`skip` is capped on every backend.** Only Elasticsearch bounded it (via its
  result window); SQL and MongoDB scanned arbitrarily deep. The new
  `query.max_skip` (default `10000`) rejects — never clamps — a larger offset
  on all three. Raise it (or `ORION_QUERY__MAX_SKIP`) if you genuinely page
  deeper (W12).
- **REST route matching is byte-exact and percent-decodes path parameters
  exactly once.** Static segments matched case-insensitively, and the data
  plane matched a path axum had already percent-decoded — so `%2F` acted as a
  segment separator before matching, a literal `/` was inexpressible inside a
  parameter, and the rate-limit middleware (which matches the raw URI) could
  resolve a different channel than the handler. Matching now splits on raw `/`
  first and decodes each segment exactly once: `/ORDERS/1` no longer matches
  `/orders/{id}` (RFC 3986 paths are case-sensitive; `%6F` still equals `o` —
  encoding an unreserved character is equivalence, not difference),
  `metadata.params` arrive decoded (`/orders/a%2Fb` yields `id == "a/b"`), and
  an invalid percent-sequence (`%ZZ`) is answered with `400` instead of being
  matched literally.

  Fix client URLs whose casing no longer matches, and drop any hand-decoding a
  workflow did on a param — decoding twice changes meaning. Route-conflict
  canonicalisation is case-preserving to match, so two casings of one path are
  two co-activatable routes. `route_pattern` also rejects `%` now, on create,
  update and import: patterns are written literally and requests match by
  their decoded value, so write the character itself. Already-active channels
  keep their (unreachable-as-written) behaviour until you next edit one (N10).
- **`backpressure.max_concurrent` is renamed `max_concurrent_per_node`.** The
  semaphore is per process, but the name read as an absolute cap while sitting
  beside dedup and rate-limit controls that *are* shared in cluster mode — two
  controls in one config block with opposite cluster semantics and no naming
  difference. The old key is accepted as a deserialization alias for one
  release, so stored configs keep working; rename it the next time you edit the
  channel (N9).
- **Async trace results are never sampled away.** `trace_storage.sample_rate`
  on the async path dropped the *result* while the pending/running/completed
  status rows were still written — the caller polled a `completed` trace with
  nothing in it, and the storage was spent anyway. `for_async_submission` now
  pins `sample_rate` to `1.0`, exactly as it upgrades `mode = "off"` to `sync`:
  a 202 is a receipt for a fetchable result. **Async trace storage for sampled
  channels will grow** — bound it with `errors_only` or a shorter
  `trace_queue.retention_hours` instead. Sampling applies in full on the sync
  path, where the draw happens once per trace at the single point persistence
  is decided and a sampled-out trace produces no rows at all (N22).
- **`kafka.max_inflight` is removed — it advertised concurrency that never
  existed.** The consumer created the semaphore, acquired a permit and then
  awaited each message inline, so concurrency was always exactly 1 whatever the
  value said; the field, its validation and the startup log line all described
  behaviour the code never had. Sequential processing is load-bearing for the
  at-least-once contract — committing an offset implicitly commits every
  earlier offset on the partition — so the honest fix is removal, not
  parallelism. A config file still carrying the key fails startup via
  `deny_unknown_fields`, and a manifest still setting
  `ORION_KAFKA__MAX_INFLIGHT` is refused at startup with the removal reason.
  Delete both; scale throughput by running more instances in the same consumer
  group (K4).
- **`orion-server validate-config` prints the full effective config, and stops
  printing database credentials.** The old output was a hand-maintained summary
  of a dozen settings that omitted `[cluster]` entirely, all the DLQ knobs,
  `[trace_storage]`, `[ingest]`, `[query]`, `[write]`, most of `[engine]` and
  `[kafka.auth]` — exactly the settings most likely to be wrong in production —
  and printed `storage.url` verbatim, embedded password included. The default
  output is now the entire merged config (defaults + file + `ORION_*`
  overrides), serialized from the same structs the server runs on, so a new
  section can never be omitted again; secrets are masked with the same policy
  as the connector API. Anything that grepped the old summary shape, or scraped
  a credential out of it, breaks: parse stdout as TOML, or pass
  `--format json`; `--format summary` restores a short human summary (now also
  masked). Under `toml` and `json` the validity note moves to stderr so stdout
  stays machine-parseable; `--format summary` keeps it on stdout. Exit codes are
  unchanged — `validate-config || exit 1` needs no edit (O15).

### Added

- **`server.max_admin_body_size`** (default 8 MB) bounds admin request bodies
  independently of the data plane. The limit was a single global layer set
  from `ingest.max_payload_size` — a name that says *data plane* — so raising
  it for a bulk import also raised it for anonymous channel traffic.

- **`query.max_skip`** (default `10000`) — hard cap on the `data_query` `skip`
  offset, enforced identically on SQL, MongoDB and Elasticsearch. A query
  skipping deeper is rejected, never clamped, exactly like `query.max_limit`.
  Override with `ORION_QUERY__MAX_SKIP` (W12).

- **`on_backend_error: "allow" | "deny"` on a channel's `rate_limit` and
  `deduplication`.** Both guards failed open on Redis errors unconditionally,
  with fail-open pinned as a trait contract — a Redis blip silently removed all
  rate limiting and all idempotency cluster-wide, and `/readyz` catches only a
  full outage. The default stays `allow` (availability wins); payment and
  idempotency workloads can opt into `deny`, which refuses with `503` — never a
  lying `409` or `429`, because the key or limit is unverifiable rather than
  violated — until the backend recovers (N7).

- **`storage.backup_retention_count`** bounds SQLite backups: after each
  successful `POST /api/v1/admin/backups` the oldest `orion_backup_*.db` files
  are pruned so at most N remain (the prune is logged, and only files matching
  the backup naming pattern are ever candidates). Backups land on the same disk
  as the live database, so an unbounded set was a backup mechanism that could
  cause the outage it exists to recover from. Unset keeps every backup — the
  previous behaviour; `0` is refused at startup, because "keep none" is not a
  retention policy. Env override `ORION_STORAGE__BACKUP_RETENTION_COUNT` (O6).

- **`orion_job_last_success_timestamp_seconds{job}`** — a gauge stamped with
  the unix time of each background job's last fully successful tick:
  `trace_cleanup`, `audit_cleanup`, `dlq_retry`, `epoch_watcher` (cluster mode)
  and `kafka_lag` (Kafka enabled). The periodic jobs deliberately swallow
  per-tick errors and keep looping, so a sustained DB blip silently stopped
  trace cleanup and DLQ retry cluster-wide with no alertable signal. Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals. In cluster mode only the lease-holding node stamps the
  lease-gated jobs — a node that loses the lease honestly goes stale rather
  than lying about freshness — and the lag poller stamps only when both the
  committed offsets and every watermark lookup answered, so a broker that
  freezes the lag gauges freezes the stamp with them (O3).

- **`/health` and `/readyz` observe Kafka ingestion.** Both probes carry a
  `kafka` component — present only when `kafka.enabled`, so non-Kafka
  deployments get byte-identical bodies — reporting `error` while ingestion is
  degraded or the consume loop has died. `/readyz` includes it in readiness, so
  a node that consumes nothing returns `503` and leaves the load-balancer
  rotation; `/health` reports `status: "degraded"` while HTTP itself keeps
  serving. The probes take the consumer handle with a non-blocking lock, so a
  routine reload restart can never stall them. The new
  `orion_kafka_ingest_degraded` gauge (0/1) carries the same signal for
  Prometheus (O10, K7).

- **`orion_build_info{version, git_hash, build_timestamp}`** — the standard way
  to answer "which build is each replica running?" from Prometheus. Previously
  that information existed only in `--version`, one boot log line, and the
  admin-gated `/health` body, none of which a scrape can join against.
- **`orion_admin_auth_failures_total{reason}`** — rejected admin credentials,
  split out from the shared `errors_total{type="auth_failure"}` so credential
  guessing can be alerted on without also matching `panic`, `dedup_backend`
  and a dozen other unrelated call sites.

### Fixed

- **`?dry_run=true` on an import now reads the database.** It performed no DB
  reads at all, as its own doc comment said. The stated use case is CI
  pre-flight and the most common real failure is a name conflict, which is
  exactly what a no-DB dry-run cannot see — so a green dry-run said nothing
  about whether the real import would work. It now reports conflicts against
  stored rows *and* duplicates within the batch; the second was free and
  previously missed entirely.

- **`POST /admin/workflows/validate` no longer green-lights payloads
  `POST /admin/workflows` rejects.** `validate_workflow_tasks_schema` carried
  the doc comment *"Public so the `/validate` endpoint can reuse it"* and had
  **zero external callers**; the endpoint re-implemented the same walk and the
  two disagreed by design — an unknown `function.name` was a hard error at
  create and a *warning* here. A linter that green-lights a rejected payload
  is worse than no linter. The create-path validator now runs first and
  verbatim; the endpoint's remaining checks are only ever additional.

- **A poisoned profile mutex no longer fails the request.** The per-request
  profiler took its locks inside the request future and `.expect()`ed them, so
  one panic anywhere poisoned the mutex for the collector's lifetime and turned
  every subsequent profiled request into an opaque 500 — with no request id and
  no security headers, because it surfaced through the panic-catch layer. The
  same layer sat behind `json_response`, which `.expect()`ed a
  `Response::builder()` result on **every successful data request**; the
  response is now assembled directly, with no `Result` to assert past.

- **A second render of a profile is no longer blank.** `to_json` drained the
  engine-lock, workflow-total and trace-store timings as it read them, and the
  sync path renders one profile for the response and another for the persisted
  trace — so the stored copy had its phase timings missing and
  `workflow_overhead_ms` recomputed from nothing.

- **`channel_call` is attributed in `by_connector`.** It passed no label, so the
  one handler whose fan-out most needs attribution showed up as unattributed
  entries with no way to tell which target was slow. Samples are now labelled
  with the target channel, static or resolved from `channel_logic`.

- **A connector task missing `connector` says so first.** The handlers resolved
  `key` / `filter` / `params` against the message before checking that a
  connector was even named, so a task missing both reported the other field —
  the author fixed that, re-ran, and only then learned about `connector`.

- **The circuit breaker now guards all nine egress paths, not just
  `http_call`.** `db_read`, `db_write`, `data_query`, `data_write`,
  `mongo_read`, `cache_read`, `cache_write` and `publish_kafka` reached their
  pools directly, so `[engine.circuit_breaker]` read as global resilience while
  a hung PostgreSQL or Redis pinned every worker.

  **Only retryable failures trip it.** A query the backend *rejected* — a syntax
  error, a constraint violation, a row-cap breach — says nothing about the
  dependency's health, and counting it would let one bad workflow trip the
  breaker on a healthy database and take down every other channel using it. The
  error taxonomy above is what makes "retryable" mean "the dependency is in
  trouble" rather than "something went wrong".

  Breaker keys keep their `channel:connector` shape, and the whole thing stays a
  no-op while `engine.circuit_breaker.enabled` is false (still the default). If
  you enable it, expect breakers for database and cache connectors that
  previously only appeared for HTTP.

- **Connector failures are classified instead of all becoming non-retryable
  500s.** Every non-HTTP connector error went through one constructor producing
  `FunctionExecution { source: None }`, which dataflow-rs classifies as **not
  retryable**. Two consequences:

  - A dead PostgreSQL, Redis or MongoDB was a non-retryable 500, while the
    *identical* HTTP outage was a retryable `Io` — so **DLQ retry policy
    diverged by backend** for no principled reason. Failures to *reach* a
    backend now produce `Io` and retry like the HTTP path; a query the backend
    rejected stays non-retryable, which is correct.
  - A caller-fixable limit reported through the 500 path, so its message was
    replaced by the generic internal-error text. `db_read`'s row cap — *"add a
    LIMIT to the query or raise the cap"* — was sanitised away exactly when the
    caller needed it. Limits are now **400** with the guidance intact.

  **`GET`-style row-cap failures change status from 500 to 400.** If you alert
  on 5xx from the data plane, a previously-500 row-cap breach now shows as a
  client error, which is what it is.

- **An async REST channel's `route_pattern` is no longer silently ignored.**
  The route table filtered to `channel_type == "sync"`, while channel validation
  *requires* a `route_pattern` for the `rest`/`http` protocols regardless of
  type. So an async REST channel was forced to declare a route, accepted with a
  201, activated cleanly — and its declared route 404'd forever, reachable only
  by channel name. REST/HTTP channels now register their route whatever their
  type; `/async` is stripped before route matching, so an async channel's
  pattern works at `POST /api/v1/data/{pattern}/async`.

- **Workflows using the `enrich` built-in were rejected at create.**
  `KNOWN_FUNCTIONS` — the list that gates workflow creation — omitted
  dataflow-rs's `enrich`, so `POST /admin/workflows` refused any task using it
  with `unknown_function`, even though the engine runs it fine. The list is now
  pinned by a test that derives the authoritative set from the engine's own
  `FunctionNotFound` message, so a dependency bump that adds or renames a
  built-in fails CI instead of silently rejecting valid workflows.

- **Circuit-breaker reads no longer present node-local state as cluster-wide.**
  `GET /admin/connectors/circuit-breakers` and `/health` returned one replica's
  breaker map unqualified. That read as cluster state precisely because its
  sibling — the *reset* — **is** cluster-aware and fans out over the epoch bus.
  Both payloads now carry `scope: "node"` and the `instance_id` whose map it is.

  Relatedly, `POST /admin/connectors/circuit-breakers/{key}` no longer returns
  **404 in cluster mode** when the key is not open on the receiving node. Breakers
  are per-replica, so the key an operator wants to clear is usually open on a
  different node than the one the load balancer picked — and the fan-out is what
  actually clears it. The response gained `found_on_this_node` to distinguish the
  two cases. Single-node deployments still 404.

- **Connector metrics are now emitted by default.** `connector_requests_total`
  and `connector_request_duration_seconds` were emitted from exactly one place
  — inside the circuit-breaker wrapper — which only `http_call` reached, and
  only when `engine.circuit_breaker.enabled` was true. That defaults to
  **`false`**, so a default install emitted **zero** connector-level request
  counts or latencies for *any* of the ten handlers: every external dependency
  was dark in Prometheus until an operator flipped an unrelated resilience flag.

  All nine connector handlers (`http_call`, `db_read`, `db_write`,
  `data_query`, `data_write`, `mongo_read`, `cache_read`, `cache_write`,
  `publish_kafka`) now record both metrics unconditionally. Observability no
  longer depends on resilience configuration.

  **Not changed:** the circuit breaker itself still only wraps `http_call`. The
  eight other egress paths reach their pools directly, so a hung Postgres or
  Redis is still not breaker-protected.

- **Retention cleanup no longer runs as one unbounded `DELETE`.** All three
  retention jobs — traces, audit logs and DLQ purge — issued a single
  `DELETE … WHERE created_at < cutoff` per tick. The first tick after enabling
  retention is then one transaction over potentially millions of rows: SQLite
  holds the write lock for its whole duration, so **every other writer hits the
  5 s `busy_timeout` and fails**; PostgreSQL bloats WAL and blocks autovacuum;
  MySQL can exceed `innodb_lock_wait_timeout`. In cluster mode the job lease
  (`interval_secs + 60`) could expire mid-delete, letting a second node start a
  duplicate.

  Deletes now run in 1 000-row chunks, yielding between them, capped at 5 000
  chunks per tick with the remainder left for the next one. The statement is
  identical on all three backends — the nested derived table is what makes
  MySQL accept a subquery over the table being deleted (error 1093).

  No configuration change and no behaviour change beyond the locking profile:
  the same rows are removed.

- **The OpenAPI document now describes every response it serves.** Measured
  against the committed `docs/openapi.json`: **44 of the 48** 2xx responses had
  no `content` block, as did **30** declared 4xx/5xx — the spec named a status
  and said nothing about its body, so generated clients got `any` where a type
  belonged. All 45 body-carrying 2xx and all 141 error responses are now typed
  (`204` stays bodiless, as it must). Two tests hold the line: one fails on any
  response that declares a status without a schema, the other on any storage row
  struct being published.

  Also corrected: `Workflow`, `Channel` and `Trace` were registered as schemas
  and referenced by **nothing**. They describe database rows — `condition_json`
  and `tasks_json` as opaque **strings** — while the endpoints return
  `WorkflowResponse`/`ChannelResponse` with those fields parsed. The row structs
  are gone from the document and the DTOs are published in their place.
  `Connector`, `TraceDlqEntry` and `AuditLogEntry` stay: their handlers do
  return them verbatim.

  This is spec-only — no endpoint changed shape. Regenerate clients to pick up
  the types.

- **A duplicate `create` answers `409`, not `500`.** `POST /admin/workflows`
  and `POST /admin/channels` with an existing id returned
  `{"code":"INTERNAL_ERROR"}` for a plain client error — connectors already
  said `409`, and the existing tests asserted only `is_err()`, so they passed
  on the wrong status. A shared `map_duplicate` helper now maps both duplicate
  shapes to `CONFLICT`: the structured `UniqueViolation` kind (Postgres'
  partial unique index, primary-key collisions on every backend) and the
  generic errors the SQLite/MySQL single-draft triggers raise, which carry no
  kind sqlx can classify and are matched on the trigger's message. **Retry
  logic that treated the 500 as transient must treat the 409 as permanent** —
  pick a different id, or use the import endpoints, which report conflicts per
  item without failing the batch (D16).

- **`GET /admin/workflows/export` no longer materialises every workflow in one
  query.** `WorkflowRepository::list` ignored the `limit`/`offset` its own
  filter carries and skipped the `timed_db_op` wrapper every sibling has — and
  it backed export, so one admin request loaded every current workflow with
  full `tasks_json` at once. `list` now honours the filter (clamped to the same
  50-default / 1000-cap as every list, with a `workflow_id` tiebreaker so
  paging cannot skip or repeat rows) and is instrumented; export pages through
  it 500 rows at a time until exhausted and still returns the complete result.

  **The export is no longer a point-in-time snapshot.** Each page is an
  independent query with no transaction spanning them, so a workflow created,
  deleted or renamed mid-export can be missed or appear twice in one response.
  Quiesce workflow mutations (or re-export until two consecutive responses
  match) if you use export as a backup (D7).

- **`claim_pending` no longer formats a runtime value into SQL text.** The DLQ
  claim was the last hand-written SQL under `src/storage/`: three backend arms
  interpolated `limit` into the statement and hand-wrote six column
  identifiers, so a rename in `schema.rs` compiled and failed only at runtime,
  on all three backends. Every arm is sea-query built now — the limit travels
  as a bound parameter, identifiers come from the `Iden` enum, and the
  exhaustion predicate is built once and shared with the DLQ list filter and
  purge. A per-backend rendered-SQL shape test pins `RETURNING`,
  `FOR UPDATE SKIP LOCKED` and the placeholder limit (D25).

- **Per-channel limiter and backpressure state survives an engine reload.**
  Every admin mutation — and, in cluster mode, every epoch resync on every node
  — rebuilt the channel registry with fresh rate limiters and semaphores,
  refilling every consumed burst and forgetting every in-flight permit, so a
  caller could bypass a per-channel limit by causing (or waiting for) a reload.
  The registry now reuses a channel's limiter while `(requests_per_second,
  burst, key_logic)` is unchanged and its semaphore while
  `max_concurrent_per_node` is unchanged (N6).

- **A failed Kafka consumer restart no longer stops ingestion permanently with
  every probe green.** Engine reload took the consumer handle out of its mutex
  and, when the restart errored, only logged — so a transient broker outage
  during any reload silenced ingestion for the process lifetime while the pod
  stayed in rotation. The restart path now flags ingestion degraded (mirrored
  to the `orion_kafka_ingest_degraded` gauge) and spawns a single-occupancy
  supervisor that retries with capped exponential backoff (1 s doubling to
  60 s), re-reading the active channel list on each attempt so topic changes
  made while ingestion was down are honoured, and standing down on recovery,
  when no topics remain, or when the node drains. The supervisor releases its
  occupancy slot while still holding the consumer-handle mutex, closing a
  window where a reload failing between the unlock and the release spawned no
  replacement and left the node degraded with no supervisor. Boot, reload and
  the supervisor now start consumers through one shared builder, so the three
  paths cannot drift (K7).

- **Rebalances no longer lose in-flight offset commits.** The consumer ran with
  rdkafka's default context — no `pre_rebalance`, no `post_rebalance`, no
  `commit_callback` — while committing asynchronously, so an unconfirmed commit
  was simply lost on revocation and failures were logged at the enqueue site
  and nowhere else. A `ConsumerContext` now flushes unconfirmed commits
  synchronously in `pre_rebalance` while the consumer still owns the
  partitions, records revoked partitions in shared state the message loop
  checks before working a message and again before committing it (abandoning it
  uncommitted for its new owner), and surfaces async commit failures through
  `commit_callback` with an `orion_errors_total{type="kafka_commit"}` count
  instead of silence (K8).

- **A failing Kafka message no longer retries its consumer out of the group.**
  The in-place retry loop blocks polling, so retrying without a cap meant
  eviction once the poll gap passed `max.poll.interval.ms` — while the consumer
  kept working, and would finally commit, a partition it no longer owned.
  Retrying in place is now bounded to 80% of `max.poll.interval.ms` (240 s
  against librdkafka's 300 s default, derived from `kafka.extra_config` when it
  sets the property). On expiry the consumer seeks the partition back to the
  message's offset and returns to the poll loop, so the message is redelivered
  — neither committed nor dropped, at-least-once intact — rebalance callbacks
  fire, and group membership is kept. Head-of-line blocking on a poison message
  is unchanged: enabling `[kafka.dlq]` remains the fix. Each expiry counts
  `orion_errors_total{type="kafka_retry_budget_exhausted"}` alongside the
  existing `kafka_retry` counter (K8).

- `default_resolvers()` is built once per connector reload instead of once per
  connector inside the load loop (N23).

- **MongoDB connectors now honour `max_connections` and `connect_timeout_ms`.**
  Both live on the same `db` connector struct the SQL path reads, and the SQL
  pool applied both while the Mongo client applied neither — so an unreachable
  Mongo host waited on the driver's 30 s server-selection default instead of
  the configured timeout, stalling the request rather than failing it. The
  timeout now caps server selection as well as connection.
- **The shipped `config.toml.example` now loads on a clean machine.**
  Placeholder substitution runs over the raw file text before TOML parsing, so
  the `${VAR}` in the header comment that *documents* the placeholder syntax —
  and three `${CONFLUENT_API_KEY}`-style examples further down — were read as
  required variables. Copying the example and starting Orion failed with
  *"Required environment variable 'VAR' is not set"*. The comments now use the
  `$$` escape, and the drift test loads the file through the real entry point
  instead of only parsing it as TOML.
- **One unusable workflow no longer takes down the whole instance.** Task input
  parsing runs inside engine construction, after the loader has decided what to
  load, so a stored row that fails it aborted the process at boot and took every
  channel on every node down on reload — defeating the per-channel quarantine.
  Unregistered function names and malformed `channel_call` inputs are now
  detected during the load and quarantine only their own channel.
- **`channel_call` accepts a `channel_logic`-only task.** The schema, docs and
  validation rule all declare `channel` optional when `channel_logic` is given,
  but the input struct required it, so such a workflow passed admin validation
  and then failed the engine build with `missing field 'channel'`.
- Unknown or archived channels return 404 on the data plane rather than a
  generic engine error.
- `channel_call` refuses a missing target instead of failing opaquely; the
  recursion depth and cycle guards are now covered by tests.
- TTL stores and circuit-breaker cooldowns use a monotonic, pausable clock, so
  a wall-clock step no longer extends or shortens either.

### Changed

- CI and CodeQL now run on `release/**` and `v*` branches. The release
  workflows require a successful CI run at the tag SHA, which no commit on a
  release branch could previously have.
- **The shipped deployment artefacts are hardened and pinned.** The Helm chart
  had no `securityContext` anywhere, failing Pod Security Standards
  `restricted` and every policy scanner out of the box; it inherited
  Kubernetes' `maxUnavailable: 25%`, which at 2 replicas removes a pod before
  its replacement is Ready and defeats the graceful-drain design; the migrate
  Job carried the full `postgres://user:pass@…` URL as a plain env value,
  visible in `kubectl get job -o yaml` and every audit sink; and the compose
  files floated on `:latest` while `docker-compose.ha.yml` set `build:`
  alongside `image:`, so `docker compose build` silently overwrote the
  published tag with a dev build.

  The Deployment and the migrate Job now run non-root with a read-only root
  filesystem, `allowPrivilegeEscalation: false`, all capabilities dropped and
  the `RuntimeDefault` seccomp profile (all values-overridable); the image
  pins its user to numeric UID/GID `10001` — the kubelet cannot verify
  `runAsNonRoot` against a named `USER` — and the chart's
  `runAsUser`/`runAsGroup`/`fsGroup` match, which also makes freshly
  provisioned PVCs writable. The read-only rootfs gets an emptyDir at `/tmp`
  and a data volume at `/app/data`, with new `persistence.*` values providing a
  kept-on-uninstall PVC for single-node SQLite installs (and `backup_dir`
  pointed at it — with a read-only rootfs, `POST /admin/backups` needs either
  `persistence.enabled` or a `storage.backup_dir` under a writable mount).
  `spec.strategy` is explicit (`maxUnavailable: 0`,
  `maxSurge: 1`), a soft pod anti-affinity spreads replicas across nodes with a
  `topologySpreadConstraints` passthrough, and a `startupProbe` on `/healthz`
  gives boot a five-minute budget before liveness takes over. The migrate Job
  reads the URL through `secretKeyRef` in both the install and the upgrade case
  via a hook-scoped copy of the storage Secret, leaving the Secret the server
  reads a normal release resource. All three compose topologies pin
  `ghcr.io/goplasmatic/orion:${ORION_VERSION:-1.0.0}`, with local HA builds
  moved to the `docker-compose.ha.build.yml` override that retags them as
  `orion:local`. Finally, `.dockerignore` excludes `.git/`, so every released
  container reported `git_hash=unknown` from `/health`, `/metrics` and
  `--version` — the Dockerfile now takes `ARG GIT_HASH`, `build.rs` prefers an
  already-set env var, and both the release and CI image builds pass the commit
  SHA (P2, P4, P5, P6, P7, P10, P11, C23).
- **CI gates licenses and supply chain, not just advisories.** `cargo audit`
  covered advisories only: no license-compatibility check across the ~600-crate
  tree, and unmaintained or yanked crates passed silently. `cargo deny check`
  replaces it against a new `deny.toml` gating advisories (carrying over the
  documented RUSTSEC-2023-0071 `rsa`/sqlx-mysql ignore), an Apache-2.0-compatible
  license allow-list, wildcard and source bans, and yanked crates — which
  surfaced and removed the yanked `spin 0.9.8`. Alongside it: Dependabot version
  updates for `cargo` and `github-actions`, weekly — cargo minor/patch bumps
  grouped with majors raised separately, Actions bumps grouped together — the
  automation `SECURITY.md` already claimed — plus a `CODEOWNERS`
  file routing every PR to the active maintainer; a pinned-mdbook build job on
  every PR with `create-missing = false`, so a dangling `SUMMARY.md` entry fails
  the build instead of fabricating an empty page; concurrency groups that cancel
  a superseded PR run instead of burning the full matrix, while branch pushes
  group by commit SHA so every pushed SHA runs to completion and the
  release-time gate always finds a completed run at the tagged SHA; and
  `tests/README.md` back in step with CI's container-test filter, which was
  missing `db_column_types_test` and `dynamic_inputs_test`
  (T12, T17, T19, T25, C17).
- `resolve_write` enforces the `TooManyRows` / `UnfilteredMutation` /
  `UnfilteredNotAllowed` guards itself, behind a `&WriteConfig`, instead of
  leaving them to the `data_write` handler — the function documented as doing
  "the whole backend-neutral transformation" was unsafe to call alone.
  Handler-visible behaviour is identical (W15).

### Removed

- **`src/storage/migration_gen.rs`.** 803 test-only lines that could not
  produce the shipped schema — no `audit_logs`, `config_epoch` or `job_leases`,
  four columns missing (`traces.task_trace_json`, `traces.access_token_hash`,
  `trace_dlq.claimed_by`, `trace_dlq.claimed_until`),
  `text`/`timestamp` on MySQL where the shipped set needs
  `varchar(n)`/`datetime` — and whose module doc instructed contributors to
  regenerate checksum-frozen migrations. CONTRIBUTING.md now documents what the
  project actually does: copy the newest backend's `001_initial.sql` and adapt
  the dialect by hand (D12).

## [1.0.0] - 2026-07-27

Multi-instance (HA) support: N replicas of `orion-server` behind a load
balancer, sharing one Postgres/MySQL + Redis, behave as a single logical
system. With `cluster.enabled = false` (the default) behavior is unchanged.
See [Scalability](https://goplasmatic.github.io/Orion/features/scalability.html)
and [Availability](https://goplasmatic.github.io/Orion/features/availability.html)
for the cluster architecture.

> **Upgrading from 0.3.0?** Read the
> [Upgrade Guide](https://goplasmatic.github.io/Orion/getting-started/upgrading.html).
> It expands every item in Breaking below into what changed, how you'll notice,
> and what to do — with the SQL and PromQL to check your deployment first.

### Security

- **Credential headers are masked before entering workflow metadata.**
  `authorization`, `cookie`, `proxy-authorization` and `x-api-key` arrive in
  `metadata.headers` as `"******"` — previously their plaintext values were
  persisted into `traces.result_json` (async) and `trace_dlq.metadata_json`.
  Header *presence* is still testable from `validation_logic`. If a channel
  used `rollout.sticky_header` with a credential header, switch to a
  non-credential header — all callers now hash to one bucket otherwise.
- **Trace reads no longer expose the submitter's request context.**
  `GET /api/v1/data/traces/{id}` strips `context.metadata` (the request
  header map) from the served message, and `GET /api/v1/data/traces` returns
  payload-free rows — `input_json`, `result_json` and `task_trace_json` are
  served only by the single-trace GET. Rows written before this release
  still hold plaintext headers at rest; the projection covers reads of them.
- **Async trace reads are scoped to the submitter.** The 202 from
  `POST /{channel}/async` now carries a one-time-shown `trace_token`;
  polling `GET /traces/{id}` requires it (`x-trace-token` header or
  `?token=`) or an admin credential. Update polling clients to pass the
  token. Sync traces and pre-upgrade rows keep the admin trust model.
  New migration adds `traces.access_token_hash` on all three backends.
- **`/health` serves topology detail only to authorized callers** when
  admin auth is enabled: anonymous callers get status, version, uptime and
  coarse per-component states; `git_hash`, `build_timestamp`,
  `workflows_loaded`, the circuit-breaker map, connector load failures and
  quarantined channels (names and reasons) require the admin key. With auth
  disabled the body is unchanged.

### Breaking

- **Rate-limit client identity is the TCP peer address.** Forwarded headers
  (`X-Forwarded-For` / `X-Real-IP`) are honored only when the peer falls inside
  the new `rate_limit.trusted_proxies` CIDR list, which is **empty by default**.
  Deployments behind a proxy, load balancer, or ingress that do not configure it
  will collapse every client into a single bucket. Applies when
  `rate_limit.enabled = true` (still `false` by default), and to per-channel
  `key_logic` expressions referencing `client_ip`. A malformed entry is a hard
  startup error even when rate limiting is disabled.
- **Metrics labels changed — dashboards and alerts will break silently.**
  `rate_limit_rejections_total` lost its unbounded `client` label and gained
  `scope` (channel name, or `admin` / `data` / `operational`). Channel-labelled
  metrics (`messages_total`, `message_duration_seconds`,
  `channel_executions_total`) now emit the literal `_unknown` for unregistered
  channels on the HTTP and queue paths. No metric was renamed or removed.
- **Channels with unparseable `config_json` or uncompilable `validation_logic`
  refuse to load, in all modes.** Previously a warning, after which the channel
  served with its validation, dedup, rate limit, cache, and backpressure guards
  silently disabled. A stored config that was quietly broken now exits the
  process at startup, and fails engine reload — plus every admin mutation that
  triggers one — with `500 CONFIG_ERROR`. Registry rebuilds are all-or-nothing,
  so a refusal leaves the running engine untouched.
- **Kafka delivery is at-least-once.** Offsets advance only on successful
  processing or a *confirmed* DLQ write. With `kafka.dlq.enabled = false` (the
  default) a poison message now blocks the consumer and retries with capped
  backoff (1s → 60s) instead of being dropped; because messages are processed
  sequentially, this halts every subscribed partition on that instance.
  **Enabling `[kafka.dlq]` is the recommended action.**
- **Data-plane error bodies are sanitized.** Entries in `errors[]` are reduced
  to a code, a fixed generic message, and an optional `task_id`; correlate via
  the top-level `request_id` (also the `x-request-id` header) and read full
  detail from the persisted trace. Cached responses store the sanitized body.
- **Response cache keys fold in method, route params, and query string.**
  Existing cached entries are orphaned — never mis-served — and expire by
  `cache.ttl_secs` (default 300s).
- **Open circuit breakers return `503 CIRCUIT_OPEN`** instead of
  `500 ENGINE_ERROR`. No `Retry-After` header. With `continue_on_error: true`
  the request still returns `200` with a sanitized `TASK_ERROR`; alert on
  `circuit_breaker_rejections_total` rather than the status code.
- **A full trace queue returns `503`** (code `SERVICE_UNAVAILABLE`, message
  `Trace queue is full …`) on the async submission path instead of blocking
  indefinitely. Sized by `queue.buffer_size` / `queue.max_queue_memory_bytes`.
- **Unimplemented secret schemes are rejected.** `vault://`, `aws-sm://`,
  `gcp-sm://`, and `azure-kv://` in connector configs were passed through and
  **used as literal passwords**; the connector is now skipped at load with an
  `ERROR` log. A connector that appeared to work was never authenticating as
  intended — rotate the credential.
- **`GET /api/v1/admin/connectors` redacts userinfo inside URL-shaped values**
  at any depth (`https://user:******@host`), which finally covers `url` and
  `brokers[]`. Credential-free URLs are still returned in full. Do not
  round-trip a connector config through `GET` → `PUT`: updates replace
  `config_json` wholesale and would persist the mask.
- **`GET /api/v1/admin/audit-logs` rejects unknown query parameters with `400`**
  instead of silently returning unfiltered results. No other endpoint changed.
- **`db_read` returns values for `float4`/`REAL` and blob columns** instead of
  `null`, and errors on genuinely undecodable columns and non-finite floats.
  Blobs stringify as UTF-8 when valid, else lowercase hex. Also affects
  `data_query` and `data_write`'s `RETURNING` path. A `null` in a result now
  means only SQL NULL.
- **Trace read endpoints require admin auth.** `GET /api/v1/data/traces` and
  `/traces/{id}` return `401` for previously-open callers when
  `admin_auth.enabled = true`. No effect when admin auth is disabled.
- **Rollout bucketing is caller-stable, not random per request** — see Added.
  A canary now exposes a stable subset of callers rather than re-drawing on
  every call; aggregate percentages are unchanged.
- **The Helm chart and HA compose default to `ORION_ENVIRONMENT=production` and require
  admin API keys.** `helm install` without `adminAuth.apiKeys` or
  `adminAuth.existingSecret` fails at template time by design
  (`devStack.enabled=true` is the dev escape hatch); `docker compose -f
  docker-compose.ha.yml up` aborts without `ORION_ADMIN_API_KEYS`. Note that
  `environment = "production"` also rejects the CORS wildcard, and the default
  `cors.allowed_origins` is `["*"]` — set explicit origins before flipping.
- **Removed:** the unread `backpressure.queue_depth` channel-config field.
  Stored configs still carrying the key deserialize normally (there is no
  `deny_unknown_fields`), so this needs no migration.
- **Removed:** the unread `cors.allowed_methods` and `cors.allowed_headers`
  channel-config fields. Only `cors.allowed_origins` was ever enforced —
  per-channel preflight is not implemented — so setting them was a silent
  no-op. Same no-migration note as above.
- **Every admin list endpoint shares one pagination envelope.**
  `GET /api/v1/admin/audit-logs` (and the new trace-DLQ list) now return the
  flat `{data, total, limit, offset}` shape the workflow/channel/connector
  lists always used, instead of a nested `{data, pagination: {…}}`.
- **Malformed admin request bodies are rejected uniformly with 400 + field
  details.** The four workflow endpoints that still surfaced axum's plain-text
  422 (`PATCH …/status`, `PUT …/rollout`, `POST …/test`,
  `POST /workflows/import`) now use the same extractor as every other admin
  route, and query-string parse failures return the standard JSON error
  envelope instead of plain text.

### Added

- HTTP connector `retry_non_idempotent` (default `false`): opt POST/PATCH back
  into the retry loop. Off by default because a timed-out POST may already
  have been applied — enable only where the endpoint honours an idempotency
  key the workflow sets in `headers`.
- Elasticsearch connector `max_response_size` (default 10 MB), matching the
  HTTP connector's cap.
- `POST /{channel}/async` responses carry `trace_token`; `GET /traces/{id}`
  accepts it via the `x-trace-token` header or a `?token=` query parameter.
- `engine.fail_on_connector_load_error` (default `false`): refuse to start when
  an enabled connector cannot be loaded, so a bad rollout fails at boot where
  the orchestrator catches it rather than at request time hours later. Startup
  only — a hot reload never takes a running process down.
- `GET /health` reports two new degraded states: `components.connectors` with
  the failures under `connectors.failed_to_load`, and `components.channels`
  with the quarantined set under `channels.quarantined`. Both keep the HTTP
  status at 200 — alert on the fields, not the status code, since a 503 would
  pull the node out of its load balancer over something nothing in flight may
  be using.
- `GET /api/v1/admin/connectors` gives every row a `load_status` (`loaded`,
  `failed`, `disabled`) plus `load_error` and `load_error_stage`.
- Environment overrides for the four settings that had none:
  `ORION_SERVER__COMPRESSION__ENABLED`,
  `ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS`,
  `ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS` and `…__DATA_RPS`. The two endpoint
  limits are optional, so their variables are three-state: unset keeps the
  config-file value, a number sets it, an empty string clears it.
- **Kafka SASL/TLS authentication** — `[kafka.auth]` (`security_protocol`,
  `sasl_mechanism`, `sasl_username`, `sasl_password`, `ssl_ca_location`) plus a
  `kafka.extra_config` passthrough for arbitrary librdkafka properties, applied
  to both the consumer and the producer. Orion can now connect to Confluent
  Cloud, MSK, Aiven, and any secured broker; previously PLAINTEXT was the only
  reachable configuration. Do not set `enable.auto.commit` via the passthrough —
  it would defeat the at-least-once guarantee below.
- **Message data in every connector function** — `db_read`, `db_write`,
  `cache_read`, `cache_write`, and `mongo_read` now resolve `{"var": "…"}`
  references in their `key`, `value`, `ttl_secs`, `params`, and `filter` inputs,
  so keys, bind parameters, and Mongo filters can depend on the message instead
  of being fixed constants. `data_query`/`data_write` share the same resolver.
  `connector` and raw `query` text stay literal by design.
- **Trace DLQ operator API** — `/api/v1/admin/trace-dlq` with paginated list
  (payload-free projection), get-by-id, requeue, and purge. Failed async traces
  were previously invisible and unreplayable.
- **Audit-log filtering** — `action`, `resource_type`, `resource_id`,
  `principal`, and time-range filters on `GET /api/v1/admin/audit-logs`; unknown
  query parameters are now rejected with 400 rather than silently ignored. The
  `details` column is populated (starting with `request_id`) — it was dead
  before, and writing to it produced malformed SQL.
- **Audit-log retention** — `audit.retention_days` (default 90, `0` keeps
  forever) with a lease-gated cleanup job. The table previously grew forever
  with no supported way to prune it.
- **Operational metrics** — `trace_dlq_depth`, `trace_dlq_retries_total`,
  `trace_queue_rejected_total{reason}`, and `trace_persistence_failures_total`.
  The three conditions most worth alerting on were previously invisible.
- **Rate-limit proxy trust** — `rate_limit.trusted_proxies` (CIDR list, empty by
  default). Forwarded headers are honoured only from listed peers; otherwise the
  TCP peer address is the client identity.
- **Hashed admin keys** — `admin_auth.api_keys` accepts `sha256:<64-hex>`
  entries so keys need not sit in config as plaintext. Plaintext still works.
- **Bounded in-memory cache** — `engine.max_memory_cache_entries` (default
  100 000, `0` = unbounded) with LRU eviction. The dedup store and response
  cache were previously unbounded maps reachable from workflow config alone.
- **`[cluster]` config section** — `enabled`, `redis_url`,
  `epoch_poll_interval_ms`, `instance_id` (auto-generated UUID when empty).
  Cluster mode requires Postgres/MySQL storage and a shared Redis; startup
  refuses SQLite.
- **Config-change propagation** — every admin mutation (channels, workflows,
  rollout, connectors, manual reload) advances a `config_epoch` row; each node
  polls it and resyncs from the DB, so a change made through any node reaches
  all nodes. This also fixes connector edits, which previously propagated to
  no other node at all. Circuit-breaker resets fan out over the same bus.
- **Cluster-shared dedup, response cache, and rate limits** — channels without
  an explicit cache connector use the shared cluster Redis for idempotency
  dedup and response caching; per-channel rate limits enforce as a shared
  Redis fixed window (~configured rate across ALL replicas combined). In
  cluster mode, a channel whose backend would silently fall back to per-node
  memory refuses to load instead.
- **DLQ claim leases + job single-flight** — DLQ retries claim rows via
  `FOR UPDATE SKIP LOCKED` leases (each entry retried by exactly one node;
  expired leases self-recover), and trace cleanup / DLQ retry acquire a job
  lease per tick so only one node runs them. New `queue.dlq_batch_size` /
  `queue.dlq_lease_secs`.
- **`storage.auto_migrate`** (default `true`) — set `false` in multi-replica
  deployments: pending migrations become a hard startup error and
  `orion-server migrate` runs as a deploy step.
- **Kafka static membership** — cluster mode sets `group.instance.id` and
  `kafka.session_timeout_ms` so rolling restarts rejoin without a full group
  rebalance; epoch-driven consumer restarts are jittered 0–5 s.
- **Postgres/MySQL storage-backend test binaries** (`storage_postgres`,
  `storage_mysql`) and a multi-node cluster test binary (`cluster`) running
  two full nodes against Postgres + Redis testcontainers in CI.
- **Helm chart** (`deploy/helm/orion`) — cluster-mode Deployment with
  readyz/healthz probes, pre-upgrade migration Job, HPA, PDB, and an optional
  throwaway dev Postgres/Redis; validated on a 3-replica kind install.
- **HA reference compose** (`docker-compose.ha.yml`) — nginx LB → 2× Orion
  (cluster mode) → shared Postgres + Redis with a one-shot migrate service,
  plus `deploy/ha/rolling-drill.sh`, a zero-downtime rolling-deploy drill.
- **Sticky canary rollouts** — the rollout bucket is now a stable hash of the
  caller identity (`engine.rollout_sticky_header`, else the forwarded client
  IP), so the same caller gets the same version on every request and replica;
  previously assignment was random per request.
- **Per-instance observability** — `service.instance.id` OTel resource
  attribute and `instance_id` on request spans in cluster mode.
- Env overrides: `ORION_CLUSTER__*`, `ORION_STORAGE__AUTO_MIGRATE`,
  `ORION_STORAGE__{MAX,MIN}_CONNECTIONS`, `ORION_STORAGE__IDLE_TIMEOUT_SECS`,
  `ORION_TRACE_QUEUE__DLQ_{RETRY_ENABLED,MAX_RETRIES,POLL_INTERVAL_SECS,BATCH_SIZE,LEASE_SECS}`,
  `ORION_KAFKA__SESSION_TIMEOUT_MS`, `ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS`,
  `ORION_KAFKA__TOPICS`, `ORION_KAFKA__DLQ__{ENABLED,TOPIC}`,
  `ORION_CORS__ALLOWED_ORIGINS`, `ORION_CHANNEL_FILTER__{INCLUDE,EXCLUDE}`,
  `ORION_ENGINE__ROLLOUT_STICKY_HEADER`.

### Fixed

- **Channel runtime controls hold (proposal N2, N5).** Responses carrying task
  errors are no longer cached, so a transient downstream failure is not pinned
  for the full TTL and replayed to every caller. A `rate_limit.key_logic` that
  does not compile now quarantines the channel, and an evaluation failure
  rejects the request with `429` — previously both fell back to `client_ip`,
  silently turning a per-tenant limit into a per-IP one.
- **Egress correctness round (proposal F8, F10–F13, F17).** `http_call` no
  longer retries non-idempotent methods by default — a timed-out POST was
  re-sent up to 3× with no idempotency key; set the new HTTP-connector
  `retry_non_idempotent` to restore the old behaviour. Retries are also
  bounded by a deadline instead of running attempts plus backoff past the
  channel timeout. `publish_kafka` now publishes to the brokers its
  connector names rather than always the globally configured cluster.
  `db_read`/`mongo_read` enforce `query.max_limit` as a hard row cap, every
  MongoDB path honours `query_timeout_ms`, Elasticsearch responses respect a
  new `max_response_size` on the ES connector, and evicted connector pools
  are closed instead of leaking their connections on every connector edit
  and cluster epoch resync.
- **Routing and rollout truth (proposal F30, F33, R5, F32).** Channels whose
  workflows cannot be built — missing or unconvertible workflow, or rollout
  percentages that don't sum to 100 — are now quarantined with the reason on
  `/health` instead of silently serving engine errors or blackholing part of
  the traffic. Workflows with unknown functions are rejected at create;
  activation requires every referenced connector to exist. The channel
  include/exclude glob matcher gained real backtracking and boot logs the
  resolved channel list when filters are configured.
- **Queue durability round (proposal Q4–Q8, N15, D4).** A DLQ backoff shift
  overflow no longer kills the retry task (`dlq_max_retries` is now bounded
  1–16); a DB error on the "mark running" write routes the message to the DLQ
  instead of dropping it with the trace stuck `pending`; failed persistence
  writes retry (50ms/250ms) before being counted and dropped, and batch
  buffers are no longer cleared on error before that retry;
  `async_workers`/`batch_workers` > 1 now actually run in parallel (per-worker
  receivers, round-robin fan-out); `trace_storage.batch_size` is bounded at
  1000 so batch flushes cannot exceed SQLite's bind limit; `task_trace_json`
  is capped by `queue.max_result_size_bytes` on both paths; and trace
  retention reclaims pending/running rows older than twice the retention
  window instead of leaking them forever.

- **Connectors authored the documented way never loaded.** `ConnectorConfig` is
  internally tagged on `type`, but the type lives in its own column and the API
  takes it as a sibling `connector_type` — so a config without a redundant
  `"type"` inside it failed to deserialize and the connector was silently
  skipped. That is the shape every example, the OpenAPI spec and any admin UI
  produce. The stored column is now the single source of truth.
- **A connector `GET` → edit → `PUT` round-trip persisted `"******"` as the
  credential.** Fields returned masked are now restored from the stored row,
  and a mask with no stored counterpart is a 400 naming the field instead of a
  silent credential overwrite.
- **Per-channel rate limits never applied to REST-routed channels.** The
  middleware matched the first path segment against channel names; for a REST
  channel that segment is the route prefix, so the limiter was never found and
  the channel fell through to the platform-wide limit. Channel resolution now
  mirrors the data handler exactly, including the `/async` suffix.
- **One broken channel made the instance unmanageable.** A channel whose stored
  config no longer parses used to fail *every* admin operation that triggers a
  reload — activate, archive, delete, rollout — with a 500, and stopped the
  cluster epoch watcher resyncing all nodes. Such channels are now quarantined
  individually: still refused at every ingress (with a 503 naming the reason,
  and routed to the DLQ on the Kafka path), but the rest of the reload
  succeeds. Boot no longer aborts over one bad row either.
- **Connectors that failed to load vanished without a signal.** Env
  substitution, JSON parsing, secret resolution and deserialization failures
  are now recorded and reported on `/health` and the admin list.
- **`ORION_RATE_LIMIT__ENABLED=true` with no config file failed startup
  validation.** `RateLimitConfig` and `AppConfig` derived `Default` while also
  carrying `#[serde(default = "…")]` attributes with different values, so "the
  default" depended on how the config was produced. Both now implement
  `Default` in terms of their `default_*` functions, and the config-docs drift
  test fails on any future divergence.
- **The async path and Kafka ingest bypassed every per-channel control.**
  CORS, `validation_logic`, deduplication, and backpressure lived only in the
  sync HTTP path, so appending `/async` to a URL defeated a channel's input
  contract, and `channel_call` skipped the target channel's guards entirely.
  All ingress paths now share one guard layer, and an async request holds its
  backpressure permit for the whole of processing, so `max_concurrent` bounds
  sync and async traffic together.
- **The response cache could serve one caller's data to another.** The key
  hashed only the request body, so for a REST channel with a path parameter and
  an empty body (`GET /orders/{id}`) every id collided onto one entry. Method,
  route parameters, and query string are now part of the key.
- **Kafka messages were lost on failure.** Offsets committed unconditionally,
  so with the DLQ disabled (the default) a workflow error, timeout, or unmapped
  topic silently discarded the message. Delivery is now at-least-once: offsets
  advance only on success or a confirmed DLQ write, and UTF-8-decode failures,
  empty payloads, and unmapped topics are dead-lettered instead of dropped.
- **Poison messages retried forever.** A failing async trace re-entered the DLQ
  as a fresh row at `retry_count = 0`, so `dlq_max_retries` could never be
  reached and each cycle inserted another `traces` row. The retry count now
  travels with the message and exhausts as documented.
- **The trace queue blocked instead of shedding.** A full buffer parked the
  request indefinitely; it now returns 503, as the configuration already
  documented.
- **Postgres DLQ retry and exhaustion silently failed.** Clearing a claim lease
  bound a TEXT parameter to a `timestamp` column (Postgres error 42804) and all
  three call sites discarded the error, so in cluster mode on Postgres entries
  never backed off, never exhausted, and were re-claimed forever.
- **SSRF protection was incomplete.** Redirects were followed without
  re-validation (reaching cloud metadata via a 302), the validated DNS result
  was discarded and re-resolved (rebinding), IPv6 private ranges were largely
  unchecked, and the Elasticsearch connector skipped validation entirely.
  Redirects are now followed manually with per-hop validation, connections are
  pinned to validated addresses, and the private-range coverage is complete.
- **Rate limiting was trivially bypassed.** The client identity came from
  unvalidated forwarded headers with no peer-address fallback, so direct
  clients shared one bucket and proxied clients could mint a new identity per
  request. See `rate_limit.trusted_proxies` above.
- **Channels with broken configuration served unguarded.** An unparseable
  `config_json` silently loaded a default (no rate limit, validation, dedup,
  backpressure, timeout, or cache) and an uncompilable `validation_logic` was
  dropped with a warning. Both now refuse to load the channel.
- **`db_read` turned unreadable columns into `null`.** `REAL`/`float4` and blob
  columns silently read back as null on every SQL read path; genuinely
  unsupported types now error rather than looking like a NULL value.
- **Unimplemented secret schemes were used as literal passwords.**
  `vault://…`, `aws-sm://…`, and friends passed through verbatim as the
  credential; they are now rejected at connector load.
- **Credentials embedded in URLs leaked through the admin API.** Masking was a
  flat key-name denylist that missed `url` and `brokers[]`, so
  `redis://:PASSWORD@host` was returned in full. Masking is recursive and
  strips userinfo from URL-shaped values at any depth.
- **A restart of cluster Redis broke every node permanently.** The shared
  connection never re-established, silently disabling distributed dedup,
  response caching, and rate limiting until pods restarted.
- **An open circuit breaker returned 500 `ENGINE_ERROR`** instead of the
  documented 503 `CIRCUIT_OPEN`, so callers could not distinguish shed load
  from a server fault and the DLQ retry classifier never saw it as retryable.
  Timeouts and 503s are now classified retryable.
- **Internal error detail leaked to anonymous callers.** Success bodies
  embedded raw upstream URLs, sqlx errors, and connector names; the data plane
  now returns a code, a generic message, and a request id, with full detail
  kept in the persisted trace.
- **Unbounded Prometheus label cardinality.** Rate-limit rejections were
  labelled with a spoofable client IP, and channel-labelled metrics accepted any
  attacker-supplied path segment.
- **`PUT /channels/{id}` ran no validation at all**, and `PUT /connectors/{id}`
  skipped config validation unless the type was resent. Both now validate
  against the stored record.
- **A CORS list mixing `"*"` with explicit origins passed validation and then
  panicked at router build**, killing the server at boot; `PATCH` was missing
  from the allowed methods, making the admin status and rollout endpoints
  unusable cross-origin.
- **Admin API keys were compared with an early length check**, leaking key
  length by timing.
- **TLS was unusable — `server.tls.enabled = true` panicked at boot.**
  `RustlsConfig::from_pem_file` failed with *"Could not automatically determine
  the process-level CryptoProvider"*: rustls 0.23 auto-selects a backend only
  when exactly one is enabled, and Orion's dependency graph enables both
  (`axum-server` + `reqwest` pull `rustls/aws-lc-rs`; `mongodb` + `sqlx` pull
  `rustls/ring`). The server now installs the `aws-lc-rs` provider explicitly
  before loading certificates. **If you tried HTTPS, hit the panic, and
  terminated TLS at a proxy instead, it works now.** Covered by new TLS
  integration tests — the test debt was the bug.
- **MySQL as Orion's own storage backend never worked** — the migration set
  used mysql-client `DELIMITER` directives, TEXT columns with defaults, and
  TEXT primary keys, none of which MySQL/sqlx accept. Rewritten with the
  VARCHAR/datetime idiom; covered by container tests.
- **Postgres storage was unusable at runtime** — models decode `i64` but
  columns were `INT4` (every repository read failed), and chrono timestamps
  were bound as TEXT, which Postgres rejects against timestamp columns. Both
  fixed (new `004_bigint_columns.sql`); covered by container tests.
- **Dedup idempotency keys are now channel-scoped** (`dedup:{channel}:{token}`)
  — raw tokens previously collided across channels sharing a backend — and a
  dedup-store outage now fails open (requests allowed, `dedup_backend` error
  metric) instead of rejecting everything with 409.
- **Trace read endpoints require admin auth** — `GET /api/v1/data/traces` and
  `/traces/{id}` return full payloads but were unauthenticated even with
  `admin_auth.enabled = true`.
- **Rolling-deploy drain** — on SIGTERM, `/readyz` now flips to 503
  immediately while the node keeps serving through `shutdown_drain_secs`
  (so the LB drains it gracefully), then stops accepting and bounds the
  in-flight wait with `server.shutdown_force_timeout_secs`. Previously TLS
  stopped accepting instantly and plain HTTP never withdrew readiness.
- **`queue.dlq_max_retries` is honored** (the enqueue path hardcoded 5) and
  values `< 1` are rejected at startup; `traces.channel_id` is now populated
  on every insert path; active-immutability triggers now exist on Postgres
  and MySQL, not just SQLite.

### Changed

- `queue.dlq_max_retries` is now validated as 1–16 and connector
  `retry.max_retries` as ≤ 16 (both are exponents in a doubling backoff);
  `trace_storage.batch_size` is capped at 1000 (the batch INSERT binds ~11
  parameters per row against SQLite's 32 766-bind statement limit). Configs
  outside these ranges are rejected at startup instead of failing at runtime.
- Workflow create/update rejects unknown `function.name` values, and workflow
  activation requires every referenced connector to exist. Both were lint
  warnings; the workflow failed at its first request instead.
- Trace retention now also deletes `pending`/`running` rows older than twice
  `trace_queue.retention_hours` — previously they were never reclaimed.
- Filesystem backups (`/api/v1/admin/backups`) return `400` in cluster mode —
  the file would land on one arbitrary node; use managed-DB snapshots/PITR.
- `docs/src/features/scalability.md` and `availability.md` rewritten around
  cluster mode (the multi-node curl-loop reload workaround is obsolete).

### Removed

- Unread `backpressure.queue_depth` channel-config field (backpressure
  rejects immediately at `max_concurrent`; there is no wait queue).

## [0.3.0] - 2026-07-18

This release introduces the portable data dialect: backend-neutral `data_query` and
`data_write` task functions that render one declarative filter/envelope format to
SQL (SQLite/PostgreSQL/MySQL), MongoDB, and Elasticsearch — so workflows can read
and write data without embedding backend-specific queries. `db_read`/`db_write`
remain available as the raw-SQL escape hatch.

### Added

- **`data_query` portable read dialect** — declarative, backend-neutral queries
  (filter, sort, pagination, projection) rendered per connector backend: SQL,
  MongoDB `find`, and Elasticsearch. Supports an inline schema registry with
  relations, and `include` for fetching nested related records with hydration.
- **`data_write` portable write dialect** — insert/update/delete/upsert with
  SQL/MongoDB/Elasticsearch parity and a cross-backend end-to-end test suite.
- **Per-operation connector gates** — db/es connector configs accept
  `operations: { read, insert, update, delete, upsert, raw_write }` (all default
  `true`), enforced by the data handlers; e.g. set `"delete": false` to make a
  connector delete-proof.
- **One-command quickstart** (`examples/quickstart.sh`), a connector-backed
  `postgres-orders` example, and Getting Started guides (CLI setup, first
  connector, AI prompt pack). All examples are linted and deployed end-to-end in CI.
- **Docs**: Dev & Prod topology pages with interactive architecture diagrams,
  terminal recordings (GIFs + asciinema), a comparison page, and a benchmark chart.
- **AI-consumable docs**: `llms.txt` and generated `llms-full.txt` published with
  the docs site, alongside the checked-in OpenAPI 3.1 spec.
- **Security & community**: `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue templates,
  CodeQL (security-extended) and cargo-audit in CI, `ADOPTERS.md`.

### Changed

- Dependency upgrades: `datalogic-rs` 5.0 → 5.1, `dataflow-rs` 3.0.1 → 3.0.2,
  `datavalue-rs` 0.2.2 → 0.2.3 (benchmarked perf-neutral), `redis` 1.2 → 1.3.
- Docker release workflow publishes to GHCR only (ACR mirror removed).

### Security

- Updated `Cargo.lock` to clear RUSTSEC-2026-0185 (`quinn-proto`).

## [0.2.0] - 2026-05-27

This release upgrades the workflow engine to dataflow-rs 3.0 / datalogic-rs 5 and
adds a large set of governance, validation, and operability features. JSONLogic
compilation now happens at engine-construction time, yielding sizeable throughput
gains (+48% on complex workflows, +120% on multi-workflow scenarios) and lower P99
latency across every benchmark scenario versus the v0.1.x baseline.

### Breaking Changes

- **Engine upgrade to dataflow-rs 3.0 + datalogic-rs 5.** JSONLogic is compiled once
  at engine build time rather than per request.
- **Connector `api_key` field removed** in favour of `api_keys`. Update any connector
  configs still using the singular field.
- **Channel/connector create & update DTOs are now strongly typed enums.** Invalid
  `channel_type`, `protocol`, or connector `type` values are rejected at
  deserialization with `400` (values remain case-insensitive; v0.1 lowercase wire
  values are still accepted).
- **Profile output is namespaced** under `_orion.profile` with `version: 1`.

### Added

- **Configurable trace storage modes** — `sync`, `async`, `batch`, or `off` — as a
  global default with per-channel override via `config.tracing`.
- **Per-request workflow profile mode** for timing/inspecting task execution.
- **Per-task execution traces** captured when a channel opts in.
- **Structured error envelope** with field-pathed `FieldError` details, plus
  collection of all protocol-required-field errors in a single response.
- **Per-function input schema validation** for workflow task functions.
- **Bulk import** for channels and connectors, with `?dry_run=true` preview.
- **Strict validation of channel `config_json` at create time.**
- **Config & connector variable substitution** — `${VAR}` / `${VAR:-default}` in
  config TOML and connector configs.
- **`env://` secret references** resolved in connector configs.
- **New CLI subcommands:** `lint`, `dry-run`, and `test-connectivity`.
- **OpenAPI coverage** for the audit, backup/restore, and functions endpoints.

### Changed

- **Performance:** roughly halved per-request CPU by sharing `AppState` via `Arc` and
  gating compression/metrics work.
- **OpenTelemetry** bumped to 0.32 / 0.33; refreshed transitive dependencies
  (`rand` 0.10.1, `tokio` 1.52, and others).
- Distributed config validation into per-struct implementations; decomposed the
  `main.rs` startup sequence; split oversized handlers and centralised admin reload,
  trace-filter, and error-mapping logic.
- Renamed `connector::types` module to `connector::config`.
- Refreshed README, `docs/`, and `tests/README`, and added v0.2.0 / v3.0.0 benchmark
  result sets alongside the v2.1.5 baseline and trace-mode comparison.

### Fixed

- Clippy lints and formatting cleaned up across the crate and test suite.

## [0.1.1] - 2026-04-11

Earlier release. See the Git history for details.

## [0.1.0]

Initial release.

[Unreleased]: https://github.com/GoPlasmatic/Orion/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/GoPlasmatic/Orion/compare/v0.3.0...v1.0.0
[0.3.0]: https://github.com/GoPlasmatic/Orion/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/GoPlasmatic/Orion/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v0.1.0
