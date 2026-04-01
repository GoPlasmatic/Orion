# Observability

[← Back to README](../README.md)

## Prometheus Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `messages_total` | Counter | `channel`, `status` | Total messages processed |
| `message_duration_seconds` | Histogram | `channel` | Processing latency |
| `active_workflows` | Gauge | — | Workflows loaded in engine |
| `errors_total` | Counter | `type` | Errors encountered |
| `http_requests_total` | Counter | `method`, `path`, `status` | HTTP requests served |
| `http_request_duration_seconds` | Histogram | `method`, `path`, `status` | HTTP request latency |
| `db_query_duration_seconds` | Histogram | `operation` | Database query latency |
| `engine_reloads_total` | Counter | `status` | Engine reload events |
| `engine_reload_duration_seconds` | Histogram | — | Engine reload latency |
| `circuit_breaker_trips_total` | Counter | `connector`, `channel` | Circuit breaker trip events |
| `circuit_breaker_rejections_total` | Counter | `connector`, `channel` | Requests rejected by open breakers |
| `channel_executions_total` | Counter | `channel` | Channel invocations |
| `rate_limit_rejections_total` | Counter | `client` | Rate-limited requests |

## Health Check

`GET /health` returns component-level status with automatic degradation detection:

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "workflows_loaded": 42,
  "components": {
    "database": "ok",
    "engine": "ok"
  }
}
```

The health check tests the database with `SELECT 1` and verifies engine availability with a configurable lock timeout. If either check fails, the endpoint returns `503 Service Unavailable` with `"status": "degraded"`.

## Engine Status

`GET /api/v1/admin/engine/status` returns a detailed breakdown:

```json
{
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "workflows_count": 42,
  "active_workflows": 38,
  "channels": ["orders", "events", "alerts"]
}
```

## Logging

- Structured JSON or pretty-printed format (configurable via `logging.format`)
- Tracing spans for request lifecycle
- Request ID propagation via `x-request-id` header
- Per-crate filtering with `RUST_LOG` (e.g., `RUST_LOG=orion=debug,tower_http=info`)
- Optional OpenTelemetry trace export via the `otel` feature flag
