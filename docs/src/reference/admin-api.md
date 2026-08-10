# Admin API

All admin endpoints are under `/api/v1/admin/`. When
[admin authentication](#authentication) is enabled, requests must include a
valid API key. Success and error bodies follow the
[response envelopes](#response-envelopes) below.

## Authentication

Admin API endpoints require an API key when `admin_auth.enabled` is true.
The server reads the key from **exactly one header** — the one named by
`admin_auth.header`, which defaults to `Authorization` (with or without a
`Bearer ` prefix):

```bash
# Default configuration (admin_auth.header = "Authorization")
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

To use a custom header instead, set `admin_auth.header` — and note this
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

## Response envelopes

Every admin 2xx body puts its payload under a top-level `data` key — one shape, so one unwrapping function works everywhere:

```json
{ "data": { "workflow_id": "wf_...", "name": "Order Processing", "...": "..." } }
```

List endpoints add the three pagination counters alongside it, and nothing else:

```json
{ "data": [ ... ], "total": 137, "limit": 50, "offset": 0 }
```

Pre-1.0 responses differed for ten handlers — the [upgrade guide](../operate/upgrading-to-1.0.md) has the full list.

Errors follow one structure across both planes — see
[Errors & Response Envelopes](./errors.md#the-error-envelope).

## Lifecycle

Both channels and workflows follow a **draft → active → archived** lifecycle:

1. **Create:** entities are created as `draft` (not loaded into the engine)
2. **Update:** only draft versions can be updated via `PUT`
3. **Activate:** `PATCH /status` with `{"status": "active"}` loads the entity into the engine
4. **New version:** `POST /versions` creates a new draft version from the active entity
5. **Archive:** `PATCH /status` with `{"status": "archived"}` removes from the engine

A channel links to a workflow via `workflow_id`. Activating a channel makes it available for data processing; activating a workflow makes its logic available to the engine.

Activation order is enforced, not merely conventional: a workflow
refuses to activate while a connector its tasks reference is missing or of the
wrong type, and a channel refuses to activate while its `workflow_id` is unset,
names a workflow that does not exist, or names one with no active version. The
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
| POST | `/api/v1/admin/workflows` | Create workflow (as draft; optional `id` field for custom IDs, optional `tags: ["..."]` selection labels read back by `?tag=` filters and package export) |
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
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked) |
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

<!-- TODO(docs2): the Export & Promotion walkthrough, the package CLI section,
and Secrets in an exported bundle move to operate/promotion.md in Phase 3
(docs-implementation-plan.md T3.2). The on_conflict matrix stays here. -->

## Export & Promotion

All three primitives export and import, so an estate can live in git rather than
only in the database: snapshot an environment, diff staging against production,
review a change before it lands, recover after one. These per-kind endpoints
are the primitive layer; the entities of one service promote together as a
**package** — see [Packages & Promotion](../topology/packages.md) for that
flow end to end.

```bash
# Snapshot an environment into version control
for kind in workflows channels connectors; do
  curl -s "$ORION/api/v1/admin/$kind/export" | jq '.data' > "estate/$kind.json"
done

# Validate the bundle before it goes anywhere (a CI runner needs no secrets)
curl -s -X POST "$ORION/api/v1/admin/workflows/import?dry_run=true" \
  -H 'Content-Type: application/json' --data @estate/workflows.json

# Promote
curl -s -X POST "$ORION/api/v1/admin/workflows/import" \
  -H 'Content-Type: application/json' --data @estate/workflows.json
```

Each `/export` emits the shape its `/import` accepts, so the round trip needs no
reshaping in between. Each export reads inside **one repeatable-read
transaction**, so the result is a consistent snapshot — rows mutated
mid-export cannot be skipped or duplicated. (On MySQL this relies on
InnoDB's default REPEATABLE READ isolation; lowering it session-wide weakens
the guarantee.)

Every entity response also carries `content_hash`: `sha256:…` over the
canonical *importable content* — the DB-owned fields (`version`, `status`,
timestamps, `rollout_percentage`) excluded. Equal hashes mean "importing one
over the other is a no-op", which is how drift is detected without comparing
bodies. Hashes are computed over stored values, so only `env://`/`vault://`-
authored entities hash identically to their masked exports.

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

### The `orion-server package` CLI

The flows above are composed, end to end, by the packaging CLI — the
recommended way to promote an estate. (Concepts and walkthrough:
[Packages & Promotion](../topology/packages.md).)

```bash
export ORION_ADMIN_TOKEN=…   # sent as the admin bearer token

# Capture a package from Dev: the tagged channels, their workflows, and every
# connector those workflows reference (closure via /dependencies)
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json

orion-server package lint  -f payments-1.4.0.json          # offline, CI gate
orion-server package plan  -s https://qa.orion.internal  -f payments-1.4.0.json
orion-server package apply -s https://qa.orion.internal  -f payments-1.4.0.json
orion-server package diff  -s https://prod.orion.internal -f payments-1.4.0.json
```

`apply` claims the package receipt as `staged`, stages every entity with
`on_conflict=new_version` (connectors → workflows → channels), activates in
dependency order with `reload=defer`, reloads the engine once, and flips the
receipt to `applied`. Re-running an identical artifact is a no-op; a changed
artifact reusing an applied version is refused — bump the package version.
Every call is stamped with `X-Orion-Change-Context: package=<name>@<version>`.

### Grouping a multi-request operation (`X-Orion-Change-Context`)

A promotion is many API calls. Send the same `X-Orion-Change-Context` header
on each — e.g. `package=payments@1.4.0` — and every audit row the operation
produces carries it under `details.change_context`, so the trail can be
filtered back into the operation that caused it. Free-form, truncated at 256
bytes. Imports additionally write one audit row per entity written, alongside
the batch summary row.

### Secrets in an exported bundle

A connector export is masked, which is what makes it safe to commit — and which
decides how a connector must be authored if it is to survive the trip:

| Authored as | Exports as | Re-imports? |
|---|---|---|
| `"token": "env://STRIPE_KEY"` | `"env://STRIPE_KEY"` | **Yes** — a reference names a variable; it is not itself a credential |
| `"token": "sk_live_..."` | `"******"` | **No** — the import is refused |

The refusal is deliberate. Importing `******` would store it as a real
credential and fail at the first request instead of here, where the operator is
looking at the file. **Author connectors with `env://` references** and bundles
round-trip cleanly; the secret then lives in the deployment environment, which
is where it belongs.

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
| GET | `/api/v1/admin/functions` | List every task function with its input-field schema (category, type, required flag, description). Used by CLI tools and IDEs for autocompletion and by workflow validators to give field-pathed errors |

## Audit Logs

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/audit-logs` | List audit log entries, newest first. Filters (AND-combined, exact match): `?action=`, `?resource_type=`, `?resource_id=`, `?principal=`; time range: `?start_time=` (inclusive) and `?end_time=` (exclusive), RFC 3339; paging: `?offset=`, `?limit=` (clamped to 1–1000, default 50). An unknown parameter returns `400` |

**What a row records.** Every admin mutation writes one, including
`POST /workflows/{id}/test` — which runs the workflow's tasks against live
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

  `client_ip` follows the `rate_limit.trusted_proxies` policy — which applies
  whether or not `rate_limit.enabled` is set — so a forged `X-Forwarded-For`
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

An async trace whose persistence keeps failing lands in the dead-letter
queue and is retried automatically with backoff (see
[Resilience](../features/resilience.md)). These endpoints are the operator
view of that queue — inspect what is stuck, put an entry back in line, or
clear out entries that will never succeed.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/trace-dlq` | List DLQ entries, paginated (`?offset=`, `?limit=`). Summaries only — the failed payload is omitted; fetch one by id for it |
| GET | `/api/v1/admin/trace-dlq/{id}` | Get one entry including the failed payload and error metadata |
| POST | `/api/v1/admin/trace-dlq/{id}/requeue` | Reset the entry to `retry_count = 0` and schedule it for immediate retry — including one already exhausted |
| POST | `/api/v1/admin/trace-dlq/purge` | Delete **exhausted** entries (retries used up). Body: `{"older_than_hours": N}` (required; `0` purges every exhausted entry). Live entries are never purged |

## Backups

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/backups` | Create a database backup (SQLite only — `VACUUM INTO` a timestamped file in `storage.backup_dir`) |
| GET | `/api/v1/admin/backups` | List backup files currently in `storage.backup_dir` |

## Packages

A **package** is the channels, workflows, and connectors of one service,
promoted between instances as a versioned unit
([Packages & Promotion](../topology/packages.md)). This is the single
package-aware surface of the admin API. Packaging itself —
computing an artifact's dependency closure, planning, staging, activating —
lives in client tooling built on the per-kind endpoints above; what the server
keeps is one **receipt** per package version, because the promotion rule
cannot be enforced without the target remembering what was applied:

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
