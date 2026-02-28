# API Reference

[← Back to README](../README.md)

## Admin API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/rules` | Create rule (as draft; optional `id` field for custom IDs) |
| GET | `/api/v1/admin/rules` | List rules — filter with `?tag=`, `?channel=`, `?status=` |
| GET | `/api/v1/admin/rules/{id}` | Get rule by ID |
| PUT | `/api/v1/admin/rules/{id}` | Update draft rule |
| DELETE | `/api/v1/admin/rules/{id}` | Delete rule |
| PATCH | `/api/v1/admin/rules/{id}/status` | Change status (active/archived) |
| GET | `/api/v1/admin/rules/{id}/versions` | List rule version history |
| POST | `/api/v1/admin/rules/{id}/versions` | Create new draft version from active rule |
| PATCH | `/api/v1/admin/rules/{id}/rollout` | Update rollout percentage |
| POST | `/api/v1/admin/rules/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/rules/import` | Bulk import rules (as drafts) |
| GET | `/api/v1/admin/rules/export` | Export rules — filter with `?tag=`, `?channel=`, `?status=` |
| POST | `/api/v1/admin/rules/validate` | Validate rule definition |
| POST | `/api/v1/admin/connectors` | Create connector |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked) |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |
| GET | `/api/v1/admin/engine/status` | Engine status (version, uptime, rules count, channels) |
| POST | `/api/v1/admin/engine/reload` | Hot-reload rules |

## Data API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/data/{channel}` | Process message synchronously |
| POST | `/api/v1/data/{channel}/async` | Submit for async processing (returns trace ID) |
| GET | `/api/v1/data/traces` | List traces — filter with `?status=`, `?channel=`, `?mode=` |
| GET | `/api/v1/data/traces/{id}` | Poll async trace result |

## Operational Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (200 OK / 503 degraded) |
| GET | `/metrics` | Prometheus metrics |

## Rule Lifecycle

Rules follow a **draft → active → archived** lifecycle:

1. **Create** — rules are created as `draft` (not loaded into the engine)
2. **Update** — only draft rules can be updated via `PUT`
3. **Activate** — `PATCH /status` with `{"status": "active"}` loads the rule into the engine
4. **New version** — `POST /versions` creates a new draft version from the active rule
5. **Archive** — `PATCH /status` with `{"status": "archived"}` removes from the engine

## Error Response Format

All error responses follow a consistent structure:

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "Rule with id '...' not found"
  }
}
```
