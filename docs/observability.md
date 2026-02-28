# Observability

[← Back to README](../README.md)

## Prometheus Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `messages_total` | Counter | `channel`, `status` | Total messages processed |
| `message_duration_seconds` | Histogram | `channel` | Processing latency |
| `active_rules` | Gauge | — | Rules loaded in engine |
| `errors_total` | Counter | `type` | Errors encountered |

## Health Check

`GET /health` returns component-level status with automatic degradation detection:

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "rules_loaded": 42,
  "components": {
    "database": "ok",
    "engine": "ok"
  }
}
```

The health check tests the database with `SELECT 1` and verifies engine availability with a 2-second lock timeout. If either check fails, the endpoint returns `503 Service Unavailable` with `"status": "degraded"`.

## Engine Status

`GET /api/v1/admin/engine/status` returns a detailed breakdown:

```json
{
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "rules_count": 42,
  "active_rules": 38,
  "channels": ["orders", "events", "alerts"]
}
```

## Logging

- Structured JSON or pretty-printed format (configurable via `logging.format`)
- Tracing spans for request lifecycle
- Request ID propagation via `x-request-id` header
- Per-crate filtering with `RUST_LOG` (e.g., `RUST_LOG=orion=debug,tower_http=info`)
