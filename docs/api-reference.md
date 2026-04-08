# API Reference

[← Back to README](../README.md)

## Admin API

### Channels

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/channels` | Create channel (as draft) |
| GET | `/api/v1/admin/channels` | List channels — filter with `?status=`, `?channel_type=`, `?protocol=` |
| GET | `/api/v1/admin/channels/{id}` | Get channel by ID |
| PUT | `/api/v1/admin/channels/{id}` | Update draft channel |
| DELETE | `/api/v1/admin/channels/{id}` | Delete channel (all versions) |
| PATCH | `/api/v1/admin/channels/{id}/status` | Change status (active/archived) |
| GET | `/api/v1/admin/channels/{id}/versions` | List channel version history |
| POST | `/api/v1/admin/channels/{id}/versions` | Create new draft version from active channel |

### Workflows

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/workflows` | Create workflow (as draft; optional `id` field for custom IDs) |
| GET | `/api/v1/admin/workflows` | List workflows — filter with `?tag=`, `?status=` |
| GET | `/api/v1/admin/workflows/{id}` | Get workflow by ID |
| PUT | `/api/v1/admin/workflows/{id}` | Update draft workflow |
| DELETE | `/api/v1/admin/workflows/{id}` | Delete workflow (all versions) |
| PATCH | `/api/v1/admin/workflows/{id}/status` | Change status (active/archived) |
| GET | `/api/v1/admin/workflows/{id}/versions` | List workflow version history |
| POST | `/api/v1/admin/workflows/{id}/versions` | Create new draft version from active workflow |
| PATCH | `/api/v1/admin/workflows/{id}/rollout` | Update rollout percentage |
| POST | `/api/v1/admin/workflows/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/workflows/import` | Bulk import workflows (as drafts) |
| GET | `/api/v1/admin/workflows/export` | Export workflows — filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/workflows/validate` | Validate workflow definition |

### Connectors

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/connectors` | Create connector |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked) |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| POST | `/api/v1/admin/connectors/reload` | Reload all connectors from DB |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |

### Engine

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/engine/status` | Engine status (version, uptime, workflows count, channels) |
| POST | `/api/v1/admin/engine/reload` | Hot-reload channels and workflows |

### Audit Logs

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/audit-logs` | List audit log entries — filter with `?action=`, `?resource_type=` |

### Backup & Restore

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/backup` | Export database backup |
| POST | `/api/v1/admin/restore` | Restore from backup |

## Data API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/v1/data/{channel}` | Process message synchronously (simple channel name) |
| `POST` | `/api/v1/data/{channel}/async` | Submit for async processing (returns trace ID) |
| `ANY` | `/api/v1/data/{path...}` | REST route matching — method + path matched against channel route patterns |
| `ANY` | `/api/v1/data/{path...}/async` | Async submission via REST route matching |
| `GET` | `/api/v1/data/traces` | List traces — filter with `?status=`, `?channel=`, `?mode=` |
| `GET` | `/api/v1/data/traces/{id}` | Poll async trace result |

## API Documentation

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/openapi.json` | OpenAPI 3.0 specification |
| GET | `/docs` | Swagger UI |

## Operational Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (200 OK / 503 degraded) — checks DB, engine, uptime, circuit breakers |
| GET | `/healthz` | Kubernetes liveness probe — always returns 200 |
| GET | `/readyz` | Kubernetes readiness probe — returns 503 if DB, engine, or startup not ready |
| GET | `/metrics` | Prometheus metrics |

## Lifecycle

Both channels and workflows follow a **draft → active → archived** lifecycle:

1. **Create** — entities are created as `draft` (not loaded into the engine)
2. **Update** — only draft versions can be updated via `PUT`
3. **Activate** — `PATCH /status` with `{"status": "active"}` loads the entity into the engine
4. **New version** — `POST /versions` creates a new draft version from the active entity
5. **Archive** — `PATCH /status` with `{"status": "archived"}` removes from the engine

A channel links to a workflow via `workflow_id`. Activating a channel makes it available for data processing; activating a workflow makes its logic available to the engine.

## Data Route Resolution

When a request arrives at `/api/v1/data/{path}`, the handler resolves the target channel in order:

1. Check for trailing `/async` suffix (async mode)
2. Try REST route table match (HTTP method + path against channel `route_pattern` values)
3. Fall back to direct channel name lookup (single path segment)

## Authentication

Admin API endpoints support bearer token or API key authentication when enabled:

```bash
# Bearer token
curl -H "Authorization: Bearer your-secret-key" http://localhost:8080/api/v1/admin/workflows

# API key via custom header
curl -H "X-API-Key: your-secret-key" http://localhost:8080/api/v1/admin/workflows
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
| `FORBIDDEN` | 403 | Access denied (e.g., CORS violation) |
| `CONFLICT` | 409 | Duplicate or conflicting state |
| `RATE_LIMITED` | 429 | Too many requests |
| `TIMEOUT` | 504 | Workflow execution exceeded timeout |
| `SERVICE_UNAVAILABLE` | 503 | Backpressure or circuit breaker open |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | Invalid content type |
| `INTERNAL_ERROR` | 500 | Internal server error |
