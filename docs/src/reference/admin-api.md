<!-- description: Every Orion admin endpoint: workflows, channels, connectors, packages, engine, audit logs and backups, with their request and response envelopes. -->
# Admin API

All admin endpoints are under `/api/v1/admin/`. When
[admin authentication](#authentication) is enabled, requests must include a
valid API key. Success and error bodies follow the
[response envelopes](#response-envelopes) below.

## Endpoint index

| Resource | Operations |
|---|---|
| [Channels](#channels) | Create, validate, activate, version, import, and export endpoints |
| [Workflows](#workflows) | Create, test, activate, roll out, version, import, and export endpoints |
| [Connectors](#connectors) | Create, validate, test, update, and circuit-breaker endpoints |
| [Packages](#packages) | Inspect applied package receipts |
| [Engine](#engine) | Inspect and reload the running engine |
| [Functions](#functions) | Discover registered task functions and schemas |
| [Audit logs](#audit-logs) | Query administrative actions |
| [Trace DLQ](#trace-dlq) | Inspect and retry failed asynchronous persistence |
| [Backups](#backups) | Create and list SQLite backups |

All resources use the [authentication](#authentication), [response
envelopes](#response-envelopes), and [lifecycle](#lifecycle) contracts below.
The generated [OpenAPI specification](./openapi.md) is the machine-readable
source for clients and code generation.

### Common errors

| Operation | Status/code | Corrective action |
|---|---|---|
| Create or update invalid JSON | `400 VALIDATION_ERROR` | Correct the field paths in `details`, then call `/validate` before writing |
| Read an unknown ID | `404 NOT_FOUND` | Verify the resource kind, ID, and target instance |
| Reuse an ID, channel name, route, or immutable package version | `409 CONFLICT` | Inspect the existing resource; create a new entity version when changing active content |
| Activate with a missing dependency or invalid transition | `400 VALIDATION_ERROR` | Run the same status request with `?dry_run=true` and resolve every reported error |
| Call without a valid admin credential | `401 UNAUTHORIZED` | Supply the configured header and key format |

See [Errors & Response Envelopes](./errors.md) for the complete registry and
response shapes. Branch on `error.code`, not the human-readable message.

## Authentication

Admin API endpoints require an API key when `admin_auth.enabled` is true.
The server reads the key from **exactly one header**: the one named by
`admin_auth.header`, which defaults to `Authorization` (with or without a
`Bearer ` prefix):

```bash
# Default configuration (admin_auth.header = "Authorization")
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

To use a custom header instead, set `admin_auth.header`, and note this
*replaces* the default, it does not add a second accepted header. With
`header = "X-API-Key"`, an `Authorization: Bearer` credential is no longer
read:

```bash
# Requires admin_auth.header = "X-API-Key" in config
curl -H "X-API-Key: your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

Configure via `[admin_auth]` in config or `ORION_ADMIN_AUTH__ENABLED=true`
environment variable. Keys listed under `admin_auth.read_only_api_keys`
authorise `GET`/`HEAD` only; every mutating method answers `403`.

### Failed-auth backoff

**Rationale.** This fixed policy limits credential guessing without adding
another security setting that can be disabled accidentally. The values below
are part of server behavior; clients only need to handle the resulting `401`
and retry conservatively.

Wrong credentials are rate-limited, so the admin plane cannot be guessed at
line speed. The policy is fixed — there is no setting for it:

| Rule | Value |
|---|---|
| Tolerated before backoff starts | 5 consecutive failures from one client |
| First lockout | 500 ms, doubling on each further failure |
| Ceiling | 30 s, so a shared NAT egress address cannot be locked out indefinitely |
| Forgotten after | 300 s with no failure from that client |

A locked-out request answers the same `401 Invalid API key` a wrong key gets:
the response never reveals that the caller is in backoff. One successful
authentication clears the budget.

Two details are easy to get wrong:

- **The client is identified by the `rate_limit.trusted_proxies` policy**, not
  by a raw `X-Forwarded-For`, so a forged header cannot mint a fresh budget per
  request. Behind an unlisted proxy every caller shares one budget. See the
  `client_ip` note under [Audit Logs](#audit-logs).
- **`GET /traces/{id}` shares the same budget.** It authenticates itself with a
  per-submission trace token rather than through the middleware, so a wrong
  token counts as a failure and a correct one clears the budget, exactly as an
  admin key does.

A read-only key refused on a mutation is a `403` and does **not** count: the
credential is valid, only its authority is not. Each outcome increments
`orion_admin_auth_failures_total` under its own `reason`. See the
[Metrics Reference](./metrics.md).

## Response envelopes

Every admin 2xx body puts its payload under a top-level `data` key — one shape, so one unwrapping function works everywhere:

```json
{ "data": { "workflow_id": "wf_...", "name": "Order Processing", "...": "..." } }
```

List endpoints add pagination counters alongside it — `limit` and `offset` always, `total` where the endpoint computes it (the trace list makes it opt-in via `?include_total=true` and adds `next_cursor`):

```json
{ "data": [ ... ], "total": 137, "limit": 50, "offset": 0 }
```

Pre-1.0 responses differed for ten handlers — the [upgrade guide](../operate/upgrading-to-1.0.md) has the full list.

### Paging and sorting by endpoint

**Rationale.** Traces use keyset paging because that table can grow without
bound. Smaller administrative collections retain offset paging. Clients should
follow the endpoint contract below rather than assuming every collection has
the same sorting controls.

Not every list takes the same query parameters. The asymmetry is contract, not
accident — the trace list pages by keyset because its table is the one that
grows without bound, and the narrower lists are the ones whose result sets are
small enough that sorting client-side is cheaper than supporting it server-side.

| Endpoints | `limit` / `offset` | `sort_by` / `sort_order` | Other |
|---|:---:|:---:|---|
| `/workflows`, `/channels`, `/connectors` and their `/export` | Yes | Yes | `?tag=`, `?status=` filters |
| `/traces` | Yes | Yes | `?cursor=` (keyset), `?include_total=true`; the response adds `next_cursor` and omits `total` unless asked |
| `/audit-logs` | Yes | No | `?start_time=` / `?end_time=` (RFC 3339 or naive), `limit` clamped to 1–1000 |
| `/trace-dlq`, `/packages`, `/{id}/versions` | Yes | No | — |

`limit` and `offset` are therefore the only two you can rely on everywhere.

Errors follow one structure across both planes. See
[Errors & Response Envelopes](./errors.md#the-error-envelope).

## Lifecycle

Both channels and workflows follow a **draft → active → archived** lifecycle:

1. **Create:** entities are created as `draft` (not loaded into the engine)
2. **Update:** only draft versions can be updated via `PUT`
3. **Activate:** `PATCH /status` with `{"status": "active"}` loads the entity into the engine
4. **New version:** `POST /versions` creates a new draft version from the active entity
5. **Archive:** `PATCH /status` with `{"status": "archived"}` removes from the engine

A channel links to a workflow via `workflow_id`. Activating a channel makes it available for data processing; activating a workflow makes its logic available to the engine.

Activation order is enforced, not merely conventional.

A workflow refuses to activate while a connector its tasks reference is missing
or of the wrong type. A channel refuses to activate while its `workflow_id` is
unset, names a workflow that does not exist, or names one with no active
version. The
working order for a bundle is therefore connectors → workflows → channels —
the same order `?dry_run=true` lets you verify before writing anything.

**Channel names are unique**: the data plane and `channel_call` address
channels by name, so a name may belong to only one `channel_id`. Create,
update and import answer `409` for a name another channel already holds
(compared against every channel's current version), and activation also
refuses a name another *active* channel holds, which covers rows created
before this rule existed. `orion-server preflight` reports pre-1.0 duplicates
before an upgrade.

## Status changes

Two query parameters compose with every status and rollout transition:

### Activation pre-flight (`dry_run`)

`PATCH /{kind}/{id}/status?dry_run=true` runs every gate the real transition
runs — draft existence, connector existence/type/MongoDB-`database` (workflows),
route collisions and the workflow-active gate (channels), rollout arithmetic —
and answers the `/validate` envelope (`{"data": {"valid", "errors",
"warnings"}}`) without writing. Gates that the real request fails as a 4xx are
reported as `errors` entries in a 200, including "not found", so one pass over
a whole bundle collects every finding instead of stopping at the first.

### Batching reloads (`reload=defer`)

Every activation, archive, and rollout change normally rebuilds the engine and
bumps the cluster config epoch — N entities promoted means N full rebuilds on
this node and N resyncs on every peer. `?reload=defer` on the status and
rollout endpoints commits the row and records the audit event but leaves the
running configuration untouched **everywhere** until `POST
/api/v1/admin/engine/reload`, which rebuilds once and bumps the epoch once.
Until that reload, the database and the running engine intentionally
disagree — a deferred activation is not serving yet. Tooling that defers must
always finish with the explicit reload; an operator making one change should
simply omit the parameter.

## Channels

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/channels` | Create channel (as draft). Optional `tags: ["..."]` — selection labels read back by `?tag=` filters and package export |
| GET | `/api/v1/admin/channels` | List channels. Filter with `?status=`, `?channel_type=`, `?protocol=`, `?tag=` |
| GET | `/api/v1/admin/channels/{id}` | Get channel by ID |
| PUT | `/api/v1/admin/channels/{id}` | Update draft channel |
| DELETE | `/api/v1/admin/channels/{id}` | Delete channel (all versions) |
| PATCH | `/api/v1/admin/channels/{id}/status` | Change status (active/archived) — see below |
| GET | `/api/v1/admin/channels/{id}/versions` | List channel version history |
| POST | `/api/v1/admin/channels/{id}/versions` | Create new draft version from active channel |
| POST | `/api/v1/admin/channels/import` | Bulk import channels (as drafts). `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing id means |
| GET | `/api/v1/admin/channels/export` | Export every matching channel, in the shape `/import` accepts. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/channels/validate` | Validate a channel definition without saving |

`PATCH /{id}/status` on a channel:

- Activation refuses a route another active channel claims.
- Activation refuses a channel whose workflow is missing or not active.
- Activation refuses a name another active channel holds.
- `?dry_run=true` and `?reload=defer` compose as described under
  [Status changes](#status-changes).

## Workflows

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/workflows` | Create workflow as a draft. Optional `workflow_id` supplies a custom ID; `tags: ["..."]` supplies selection labels used by `?tag=` filters and package export |
| GET | `/api/v1/admin/workflows` | List workflows. Filter with `?tag=`, `?status=` |
| GET | `/api/v1/admin/workflows/{id}` | Get workflow by ID |
| PUT | `/api/v1/admin/workflows/{id}` | Update draft workflow |
| DELETE | `/api/v1/admin/workflows/{id}` | Delete workflow (all versions) |
| PATCH | `/api/v1/admin/workflows/{id}/status` | Change status (active/archived). Activation refuses missing/mistyped connector references. `?dry_run=true` / `?reload=defer` — see [Status changes](#status-changes) |
| GET | `/api/v1/admin/workflows/{id}/versions` | List workflow version history |
| POST | `/api/v1/admin/workflows/{id}/versions` | Create new draft version from active workflow |
| PATCH | `/api/v1/admin/workflows/{id}/rollout` | Update rollout percentage. `?reload=defer` commits without rebuilding the engine |
| POST | `/api/v1/admin/workflows/{id}/test` | Dry-run on sample payload |
| GET | `/api/v1/admin/workflows/{id}/dependencies` | What the tasks reference: connector names (with the referencing function) and static `channel_call` targets, plus a flag when targets resolve dynamically. For closure tooling |
| POST | `/api/v1/admin/workflows/import` | Bulk import workflows (as drafts). `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing id means |
| GET | `/api/v1/admin/workflows/export` | Export workflows. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/workflows/validate` | Validate workflow definition |

## Connectors

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/connectors` | Create connector. String fields may use `env://VAR_NAME` to pull values from the process environment. Optional `tags: ["..."]` (selection labels for `?tag=` and package export) and `enabled` (default `true`; a disabled connector is never loaded into the registry, and export → import preserves the flag) |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked). Filter with `?tag=` |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked). The config comes back both parsed (`config`) and as the stored string (`config_json`) — [which to read](./connectors.md#definition-and-identity) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| POST | `/api/v1/admin/connectors/import` | Bulk import connectors. `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing name means (connectors are unversioned, so `new_version` updates in place) |
| GET | `/api/v1/admin/connectors/export` | Export every matching connector, secrets masked. Filter with `?tag=` |
| POST | `/api/v1/admin/connectors/validate` | Validate a connector definition without saving |
| POST | `/api/v1/admin/connectors/{id}/test` | Probe the connector's backend and report whether it is reachable |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |

Connector types: `http`, `kafka`, `db` (PostgreSQL/MySQL/SQLite/MongoDB), `cache`, `es` (Elasticsearch). Every connector config accepts an optional `operations` block that en/disables operation types per connector — `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` on `db` and `es`, `read` / `write` on `cache`, `publish` on `kafka`, and a `methods` allow-list on `http`. Per-type fields and gates are specified in [Connector Types](./connectors.md#operation-gates).

### Testing a connector

`POST /api/v1/admin/connectors/{id}/test` probes the saved connector's backend,
so wrong credentials surface when they are saved rather than at the first real
request. It reads the **stored row** with its `env://` references resolved, not
the registry — a connector that failed to load has no registry entry, and that
is exactly when this endpoint is useful.

```json
{ "data": { "reachable": true, "supported": true, "connector_type": "db", "probe": "SELECT 1" } }
```

A backend that cannot be reached is still a `200`: the probe ran, and
`reachable: false` with an `error` string is its answer — the failure is the
backend's, not Orion's. For the kinds with no probe
(`es`, `kafka`, and a `db` connector pointing at MongoDB via a `mongodb://`
URL), `supported: false` distinguishes the permanent capability gap from an
outage — key monitoring on `supported && !reachable`, not on `reachable`
alone.

| Type | Probe | Touches the backend? |
|---|---|---|
| `db` (SQL) | `SELECT 1` through the shared pool | Yes, read-only |
| `db` (MongoDB) | not implemented (`supported: false`) | No |
| `cache` | reads one probe key | Yes, read-only — nothing is written |
| `http` | `GET` the configured URL with the connector's auth, 5 s timeout | **Yes — one real request** |
| `es`, `kafka` | not implemented (`supported: false`) | No |

The HTTP probe issues a genuine request with genuine credentials, which is the
point: a wrong bearer token is invisible until traffic hits it. A `401`/`403` is
reported as **not** reachable — the host answered, but the connector's
credentials are wrong, and that is the failure the endpoint exists to surface.
It goes through the same client and SSRF policy as a real `http_call`, so a
probe cannot pass where traffic would fail. Every call is written to the audit
log.

Kafka brokers are covered by `orion-server test-connectivity`.

## Export & Promotion

All three primitives export and import, so an estate can live in git rather than
only in the database. Each `/export` emits the shape its `/import` accepts, so
the round trip needs no reshaping in between.

Every `/import` endpoint accepts at most **1000 items per request** and answers
`400 VALIDATION_ERROR` above that — split a larger estate into batches. The
request is also bounded by `server.max_admin_body_size`, which a batch of large
workflows can reach well before the item cap does.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/{workflows,channels,connectors}/export` | Export every entity of that kind. `?tag=` and `?status=` narrow the set |
| POST | `/api/v1/admin/{workflows,channels,connectors}/import` | Bulk import. `?on_conflict=` selects the collision policy; `?dry_run=true` reports what would happen |

Each export reads inside **one repeatable-read transaction**, so the result is a
consistent snapshot — rows mutated mid-export cannot be skipped or duplicated.
(On MySQL this relies on InnoDB's default REPEATABLE READ isolation; lowering it
session-wide weakens the guarantee.)

Every entity response also carries `content_hash`: `sha256:…` over the canonical
*importable content*, with the DB-owned fields (`version`, `status`, timestamps,
`rollout_percentage`) excluded. Equal hashes mean "importing one over the other
is a no-op", which is how drift is detected without comparing bodies. Hashes are
computed over stored values, so only `env://`/`vault://`-authored entities hash
identically to their masked exports.

The operator's guide to using these — the `orion-server package` verbs, the
receipt model, secrets handling, and mid-apply failure modes — is
[Promote Between Environments](../operate/promotion.md).

### Promoting over an existing estate (`on_conflict`)

By default an import is create-only: an item whose `workflow_id` /
`channel_id` / connector `name` is already stored becomes one `errors[]`
entry. `?on_conflict=` selects what "already stored" means instead:

| Mode | Existing draft | Existing active | Identical content | Connectors (unversioned) |
|---|---|---|---|---|
| `fail` (default) | refused | refused | refused | refused |
| `skip` | `skipped` | `skipped` | `skipped` | `skipped` |
| `new_version` | draft replaced (`updated_draft`) | new draft version cut with the item's content (`new_version`) | nothing written (`unchanged`) | updated in place (`updated`) |

Content comparison excludes the DB-owned fields (`version`, `status`,
timestamps, `rollout_percentage`), so re-importing an unmodified export
reports `unchanged` for everything — **re-running the same artifact is a
no-op**, which is what makes the import safe to retry from CI. An *archived*
entity with identical content still gets a new draft version: the point of
re-importing it is to activate it again. The response's `results` array
carries one `{index, id, action}` per non-failed item; `?dry_run=true`
composes with every mode and reports the action the real import would take.

The two upsert-ish modes refuse an id that appears twice in one batch — the
second item would silently rewrite what the first just staged.

### Grouping a multi-request operation (`X-Orion-Change-Context`)

A promotion is many API calls. Send the same `X-Orion-Change-Context` header
on each — e.g. `package=payments@1.4.0`, and every audit row the operation
produces carries it under `details.change_context`, so the trail can be
filtered back into the operation that caused it. Free-form, truncated at 256
bytes. Imports additionally write one audit row per entity written, alongside
the batch summary row.

### Secrets in an exported bundle

A connector export is masked, which is what makes it safe to commit. Only a
connector authored with an `env://` or `vault://` reference round-trips: a
literal credential exports as `"******"` and is **refused** on import, rather
than stored as a credential that fails at the first request. The rules are in
[Connector Types › Secret masking](./connectors.md#secret-masking), and what
they mean for promotion is in
[Promote Between Environments](../operate/promotion.md#secrets-survive-the-trip--if-authored-as-references).

`POST /{kind}/validate` runs the same validator `POST /{kind}` runs, so
`valid: true` means create would accept the payload — it is never laxer. An
`env://` reference that is unset on the validating host is a **warning**, not an
error, so a CI runner holding no production secrets can still check a bundle.

## Engine

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/engine/status` | Engine status (version, uptime, workflows count, channels) |
| POST | `/api/v1/admin/engine/reload` | Hot-reload channels and workflows |

## Functions

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/functions` | The catalogue of every task function a workflow may name, with the input-field schema of each one that declares one (category, type, required flag, description). `source` is `orion` for a handler Orion input-validates at create time and `engine` for a dataflow-rs built-in, which carries no `input_fields`. Used by CLI tools and IDEs for autocompletion and by workflow validators to give field-pathed errors |

## Audit Logs

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/audit-logs` | List audit log entries, newest first. Filters (AND-combined, exact match): `?action=`, `?resource_type=`, `?resource_id=`, `?principal=`; time range: `?start_time=` (inclusive) and `?end_time=` (exclusive), RFC 3339; paging: `?offset=`, `?limit=` (clamped to 1–1000, default 50). An unknown parameter returns `400` |

**What a row records.** Every admin mutation writes one, including
`POST /workflows/{id}/test`, which runs the workflow's tasks against live
connectors and so is a side-effecting operation, not a dry run.

- `principal` — the actor. `key-<16 hex>` for an authenticated caller, or
  `anonymous` when `admin_auth.enabled = false`. The id is derived as
  `SHA-256("orion:audit:key-id:v1" ‖ SHA-256(key))`, truncated to 8 bytes: it
  is stable for a given key (the same value whether the key is configured in
  plaintext or `sha256:` form), distinct for keys that share a prefix, and
  cannot be reversed to the key. Hold the config and you can recompute it to
  map a row back to a key you issued; nobody else can go in either direction.
- `details` — a JSON object with the request context: `request_id` (the same
  value as the `x-request-id` header and `error.request_id`), `client_ip`,
  `user_agent`, and `change_context` when the request carried an
  `X-Orion-Change-Context` header — free-form, truncated at 256 bytes;
  promotion tooling stamps `package=<name>@<version>` on every call of an
  apply so the trail groups the whole operation. Both attacker-controlled inputs are truncated before storage
  (256 bytes for `user_agent`, 200 for a supplied `x-request-id`). Fields that
  are unavailable are omitted rather than recorded empty.

  `client_ip` follows the `rate_limit.trusted_proxies` policy, which applies
  whether or not `rate_limit.enabled` is set, so a forged `X-Forwarded-For`
  cannot dictate it. The flip side is that with the **default empty list**,
  forwarded headers are ignored entirely and the recorded address is the
  direct peer: behind an ingress or load balancer that is the proxy's address
  on every row. List your proxies in `rate_limit.trusted_proxies` to record
  the real client.

Rows are written asynchronously so admin responses never wait on the INSERT,
but the queue is bounded (`audit.max_pending`) and drained at shutdown
(`audit.drain_timeout_secs`) — a mutation accepted moments before `SIGTERM` is
still recorded. Anything that does not make it is counted in
`orion_audit_events_dropped_total`.

## Trace DLQ

An async submission that fails lands in the dead-letter queue and is retried
automatically with backoff (see
[Timeouts, Retries & Circuit Breakers](../operate/failure-handling.md)). Two
different failures put it there, and the queue does not distinguish them:

- **The run failed**: a task errored, or the workflow exceeded the channel's
  timeout. The trace is settled `failed` and the whole submission is queued for
  re-execution, so a downstream outage that has since recovered drains by
  itself.
- **The run never started**: the trace's pre-run status write failed, so the
  message is queued rather than dropped and re-runs once the database recovers.

A *result* write that fails after a successful run is the one failure that does
**not** queue: the work is already done, so the trace is settled `failed` with
`Result persistence failed after retries` and re-running it would repeat every
side effect. Only `/async` traffic reaches this queue at all — a sync request
carries its own failure back to the caller, with nothing left to retry.

These endpoints are the operator view of that queue — inspect what is stuck,
put an entry back in line, or clear out entries that will never succeed.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/trace-dlq` | List DLQ entries, paginated (`?offset=`, `?limit=`). Summaries only — the failed payload is omitted; fetch one by id for it |
| GET | `/api/v1/admin/trace-dlq/{id}` | Get one entry including the failed payload and error metadata |
| POST | `/api/v1/admin/trace-dlq/{id}/requeue` | Reset the entry to `retry_count = 0` and schedule it for immediate retry — including one already exhausted |
| POST | `/api/v1/admin/trace-dlq/purge` | Delete **exhausted** entries (retries used up). Body: `{"older_than_hours": N}` (required; `0` purges every exhausted entry). Live entries are never purged |

## Backups

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/backups` | Create a database backup (SQLite only — `VACUUM INTO` a timestamped file in `storage.backup_dir`) — `400` in cluster mode |
| GET | `/api/v1/admin/backups` | List backup files currently in `storage.backup_dir` — `400` in cluster mode |

## Packages

A **package** is the channels, workflows, and connectors of one service,
promoted between instances as a versioned unit
([Promote Between Environments](../operate/promotion.md)). This is the single
package-aware surface of the admin API.

Packaging itself lives in client tooling built on the per-kind endpoints above:
computing an artifact's dependency closure, planning, staging, activating. What
the server keeps is one **receipt** per package version. It has to, because the
promotion rule cannot be enforced unless the target remembers what was applied:

> **An applied package version is immutable.** The same version arriving with
> a different content hash is refused with a `409`; only a `staged` receipt
> may change; any content change rides a package version bump.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/packages` | List receipt rows (paginated, `?limit=`/`?offset=`), ordered by package name, newest first within a package |
| GET | `/api/v1/admin/packages/{name}` | One package's receipts, plus `current` — the newest `applied` version |
| PUT | `/api/v1/admin/packages/{name}` | Record or advance a receipt. Body: `{"version", "content_hash", "state": "staged"\|"applied"}` |

The intended apply sequence: **claim** the receipt as `staged` (the atomic
same-version-different-content rejection, doubling as a guard against two
concurrent applies), stage the artifact's entities via the `/import`
endpoints, activate them in dependency order (connectors → workflows →
channels), then flip the receipt to `applied`. A failed apply leaves the
receipt `staged`, so a corrected re-run at the same version is legal — only a
draft can be updated. Re-putting an *older* applied version with its own
original hash is also legal and simply makes it current again (the rollback
path: entities roll forward carrying the old content; nothing moves backward).

Receipts never touch the engine — no reload, no cluster epoch bump. `state`,
`content_hash` and `principal` are recorded verbatim; the hash is opaque to
the server and compared only for equality.

## Errors

Every error code, the shared error envelope, field-pathed validation
`details`, and the two validation warnings are specified in
[Errors & Response Envelopes](./errors.md).

## Related

- [Errors & Response Envelopes](./errors.md): every code these endpoints
  return, and the envelope they return it in.
- [Promote Between Environments](../operate/promotion.md): the operator's
  guide to the export and import endpoints above.
- [OpenAPI Specification](./openapi.md): the generated contract, and where to
  fetch it.
- [The Entity Lifecycle](../concepts/lifecycle.md): the rules the status
  endpoints enforce.
