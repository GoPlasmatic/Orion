# API Reference

[← Back to README](../README.md)

## Admin API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/rules` | Create rule (optional `id` field for custom IDs) |
| GET | `/api/v1/admin/rules` | List rules — filter with `?tag=`, `?channel=`, `?status=` |
| GET | `/api/v1/admin/rules/{id}` | Get rule by ID (includes `version_count`) |
| PUT | `/api/v1/admin/rules/{id}` | Update rule (creates new version) |
| DELETE | `/api/v1/admin/rules/{id}` | Delete rule |
| PATCH | `/api/v1/admin/rules/{id}/status` | Change status (active/paused/archived) |
| POST | `/api/v1/admin/rules/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/rules/import` | Bulk import rules |
| GET | `/api/v1/admin/rules/export` | Export rules — filter with `?tag=`, `?channel=`, `?status=` |
| POST | `/api/v1/admin/connectors` | Create connector |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked) |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| GET | `/api/v1/admin/engine/status` | Engine status (version, uptime, rules count, channels) |
| POST | `/api/v1/admin/engine/reload` | Hot-reload rules |

## Data API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/data/{channel}` | Process message synchronously |
| POST | `/api/v1/data/{channel}/async` | Submit for async processing (returns job ID) |
| GET | `/api/v1/data/jobs/{id}` | Poll async job result |
| POST | `/api/v1/data/batch` | Process a batch of messages |

## Operational Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (200 OK / 503 degraded) |
| GET | `/metrics` | Prometheus metrics |

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
