# Admin API

All admin endpoints are under `/api/v1/admin/`. When admin authentication is enabled, requests must include a valid bearer token or API key.

## Success Response Format

Every admin 2xx body puts its payload under a top-level `data` key — one shape, so one unwrapping function works everywhere:

```json
{ "data": { "workflow_id": "wf_...", "name": "Order Processing", "...": "..." } }
```

List endpoints add the three pagination counters alongside it, and nothing else:

```json
{ "data": [ ... ], "total": 137, "limit": 50, "offset": 0 }
```

This is uniform as of 1.0. Before that, ten handlers — engine status and reload, the circuit-breaker list and reset, DLQ purge, workflow test and validate, the three bulk imports — returned their fields bare at the top level. See the [upgrade guide](../getting-started/upgrading.md) for the full list.

## Channels

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/channels` | Create channel (as draft) |
| GET | `/api/v1/admin/channels` | List channels. Filter with `?status=`, `?channel_type=`, `?protocol=` |
| GET | `/api/v1/admin/channels/{id}` | Get channel by ID |
| PUT | `/api/v1/admin/channels/{id}` | Update draft channel |
| DELETE | `/api/v1/admin/channels/{id}` | Delete channel (all versions) |
| PATCH | `/api/v1/admin/channels/{id}/status` | Change status (active/archived) |
| GET | `/api/v1/admin/channels/{id}/versions` | List channel version history |
| POST | `/api/v1/admin/channels/{id}/versions` | Create new draft version from active channel |
| POST | `/api/v1/admin/channels/import` | Bulk import channels (as drafts). `?dry_run=true` validates without writing |
| GET | `/api/v1/admin/channels/export` | Export every matching channel, in the shape `/import` accepts |
| POST | `/api/v1/admin/channels/validate` | Validate a channel definition without saving |

## Workflows

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/workflows` | Create workflow (as draft; optional `id` field for custom IDs) |
| GET | `/api/v1/admin/workflows` | List workflows. Filter with `?tag=`, `?status=` |
| GET | `/api/v1/admin/workflows/{id}` | Get workflow by ID |
| PUT | `/api/v1/admin/workflows/{id}` | Update draft workflow |
| DELETE | `/api/v1/admin/workflows/{id}` | Delete workflow (all versions) |
| PATCH | `/api/v1/admin/workflows/{id}/status` | Change status (active/archived) |
| GET | `/api/v1/admin/workflows/{id}/versions` | List workflow version history |
| POST | `/api/v1/admin/workflows/{id}/versions` | Create new draft version from active workflow |
| PATCH | `/api/v1/admin/workflows/{id}/rollout` | Update rollout percentage |
| POST | `/api/v1/admin/workflows/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/workflows/import` | Bulk import workflows (as drafts). `?dry_run=true` validates without writing |
| GET | `/api/v1/admin/workflows/export` | Export workflows. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/workflows/validate` | Validate workflow definition |

## Connectors

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/connectors` | Create connector. String fields may use `env://VAR_NAME` to pull values from the process environment |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked) |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| POST | `/api/v1/admin/connectors/import` | Bulk import connectors. `?dry_run=true` validates without writing |
| GET | `/api/v1/admin/connectors/export` | Export every connector, secrets masked |
| POST | `/api/v1/admin/connectors/validate` | Validate a connector definition without saving |
| POST | `/api/v1/admin/connectors/{id}/test` | Probe the connector's backend and report whether it is reachable |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |

Connector types: `http`, `kafka`, `db` (PostgreSQL/MySQL/SQLite/MongoDB), `cache`, `es` (Elasticsearch). Every connector config accepts an optional `operations` block that en/disables operation types per connector — `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` on `db` and `es`, `read` / `write` on `cache`, `publish` on `kafka`, and a `methods` allow-list on `http`. See [Operation Gates](../features/extensibility.md#operation-gates).

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
`reachable: false` with an `error` string is its answer. A `5xx` would claim
Orion failed, which is a different thing. For the types with no probe
(`es`, `kafka`), `supported: false` distinguishes the permanent capability
gap from an outage — key monitoring on `supported && !reachable`, not on
`reachable` alone.

| Type | Probe | Touches the backend? |
|---|---|---|
| `db` | `SELECT 1` through the shared pool | Yes, read-only |
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
only in the database: snapshot an environment, diff staging against production,
review a change before it lands, recover after one.

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
reshaping in between. Exports are **not** a consistent snapshot: pages are
independent queries, so rows mutated mid-export can be skipped or duplicated.
Export from a quiet instance if that matters.

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
  value as the `x-request-id` header and `error.request_id`), `client_ip` and
  `user_agent`. Both attacker-controlled inputs are truncated before storage
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

## Lifecycle

Both channels and workflows follow a **draft → active → archived** lifecycle:

1. **Create:** entities are created as `draft` (not loaded into the engine)
2. **Update:** only draft versions can be updated via `PUT`
3. **Activate:** `PATCH /status` with `{"status": "active"}` loads the entity into the engine
4. **New version:** `POST /versions` creates a new draft version from the active entity
5. **Archive:** `PATCH /status` with `{"status": "archived"}` removes from the engine

A channel links to a workflow via `workflow_id`. Activating a channel makes it available for data processing; activating a workflow makes its logic available to the engine.

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

## Error Response Format

All error responses follow a consistent structure:

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "Workflow with id '...' not found"
  }
}
```

Every code the server emits, on both the admin and data planes:

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `VALIDATION_ERROR` | 400 | Invalid input — malformed body, failed strict validation, bad query parameter |
| `UNAUTHORIZED` | 401 | Missing or invalid credentials |
| `FORBIDDEN` | 403 | Access denied — e.g. a read-only admin key on a mutating method, or a channel auth failure |
| `NOT_FOUND` | 404 | Resource not found |
| `METHOD_NOT_ALLOWED` | 405 | The path exists but not for this HTTP method |
| `CONFLICT` | 409 | Duplicate or conflicting state — e.g. a second draft, or an import collision |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | Invalid content type |
| `RATE_LIMITED` | 429 | Too many requests |
| `INTERNAL_ERROR` | 500 | Internal server error |
| `ENGINE_ERROR` | 500 | Workflow execution failed inside the engine for a reason the server does not surface (the detail is in the server log) |
| `STORAGE_ERROR` | 500 | A database operation failed (detail in the server log) |
| `SERIALIZATION_ERROR` | 500 | A stored row could not be decoded or a response could not be serialized — a server-side fault, never client input (detail in the server log) |
| `CONFIG_ERROR` | 500 | A configuration problem surfaced at request time (detail in the server log) |
| `RESPONSE_TOO_LARGE` | 500 | A connector's response exceeded the operator-configured size cap — the request cannot succeed until the cap or the response changes |
| `SERVICE_UNAVAILABLE` | 503 | Backpressure shed the request, a guard's backend failed closed, or the service is shutting down |
| `CIRCUIT_OPEN` | 503 | The target connector's circuit breaker is open; retry after it recovers |
| `TIMEOUT` | 504 | Workflow execution exceeded the channel's timeout |

When a workflow, channel, or connector fails strict validation on create/update, the envelope is extended with a `details` array of field-pathed errors (kept omitted for single-message errors so v0.1 clients aren't broken):

```json
{
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Workflow validation failed",
    "details": [
      { "path": "tasks[0].function.input.connector", "code": "REQUIRED", "message": "is required" },
      { "path": "tasks[2].function.input.method",    "code": "INVALID",  "message": "expected string, got number" }
    ]
  }
}
```

The `path` mirrors the JSON structure the API received, so editors can jump straight to the failing key. The same envelope is returned by `POST /workflows/validate`, `POST /workflows/{id}/test`, and the `orion-server lint` / `dry-run` CLI subcommands.

### Warnings

`POST /workflows/validate` returns `{ "valid", "errors", "warnings" }`. `valid` reflects `errors` only — it means "`POST /workflows` would accept this" — so a workflow can be valid and still carry warnings. Two are reported:

| Warning | Meaning |
|---|---|
| `Connector '…' not found in registry` | The task names a connector that does not exist yet. Not an error at create time (connectors and workflows may be authored in either order), but **activation refuses it**. |
| `reads '…', which no earlier task writes` | A `data.*` path read by a task that no earlier task writes. |

The second one exists because the failure it predicts is invisible at runtime: JSONLogic resolves an unknown `var` to null, so a mistyped path leaves the task running, the workflow succeeding, and the caller receiving a `200` with the field quietly missing.

It is advisory in both directions. Writes are tracked from `parse_json`/`parse_xml` targets, `map` mapping paths, and connector `output` paths, and matched by prefix — writing `data.order` covers a read of `data.order.total`. Reads of `metadata.*`, `payload`, and the element rebinding inside `map`/`reduce` bodies are out of scope and never warn. A value that legitimately arrives another way — a connector response shape, a `continue_on_error` predecessor — can still be flagged, which is why it never blocks creation.
