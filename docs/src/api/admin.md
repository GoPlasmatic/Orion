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
| POST | `/api/v1/admin/connectors/reload` | Reload all connectors from DB |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |

Connector types: `http`, `kafka`, `db` (PostgreSQL/MySQL/SQLite/MongoDB), `cache`, `es` (Elasticsearch). Every connector config accepts an optional `operations` block that en/disables operation types per connector — `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` on `db` and `es`, `read` / `write` on `cache`, `publish` on `kafka`, and a `methods` allow-list on `http`. See [Operation Gates](../features/extensibility.md#operation-gates).

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

Admin API endpoints support bearer token or API key authentication when enabled:

```bash
# Bearer token (default header: Authorization)
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows

# API key via custom header
curl -H "X-API-Key: your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

Configure via `[admin_auth]` in config or `ORION_ADMIN_AUTH__ENABLED=true` environment variable.

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

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `NOT_FOUND` | 404 | Resource not found |
| `BAD_REQUEST` | 400 | Invalid input |
| `UNAUTHORIZED` | 401 | Missing or invalid credentials |
| `FORBIDDEN` | 403 | Access denied |
| `CONFLICT` | 409 | Duplicate or conflicting state |
| `RATE_LIMITED` | 429 | Too many requests |
| `TIMEOUT` | 504 | Workflow execution exceeded timeout |
| `SERVICE_UNAVAILABLE` | 503 | Backpressure or circuit breaker open |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | Invalid content type |
| `INTERNAL_ERROR` | 500 | Internal server error |

When a workflow, channel, or connector fails strict validation on create/update, the envelope is extended with a `details` array of field-pathed errors (kept omitted for single-message errors so v0.1 clients aren't broken):

```json
{
  "error": {
    "code": "BAD_REQUEST",
    "message": "Workflow validation failed",
    "details": [
      { "field": "tasks[0].function.input.connector", "message": "is required" },
      { "field": "tasks[2].function.input.method",    "message": "expected string, got number" }
    ]
  }
}
```

The `field` path mirrors the JSON structure the API received, so editors can jump straight to the failing key. The same envelope is returned by `POST /workflows/validate`, `POST /workflows/{id}/test`, and the `orion-server lint` / `dry-run` CLI subcommands.
