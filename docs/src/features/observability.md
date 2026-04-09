# Observability

Orion provides structured logging, Prometheus metrics, distributed tracing, and health monitoring out of the box. No sidecars, no agents — everything runs inside the single binary.

## Structured Logging

Orion emits structured logs in JSON or pretty-printed format, configurable at runtime:

```toml
[logging]
level = "info"        # trace, debug, info, warn, error
format = "pretty"     # pretty or json
```

**JSON format** is recommended for production — it integrates directly with log aggregators like Loki, Datadog, or CloudWatch:

```bash
ORION_LOGGING__FORMAT=json
ORION_LOGGING__LEVEL=info
```

**Per-crate filtering** with `RUST_LOG` gives fine-grained control:

```bash
RUST_LOG=orion=debug,tower_http=warn,sqlx=warn
```

| Level | Usage |
|-------|-------|
| `error` | Failures that need attention |
| `warn` | Degraded behavior (circuit breakers, retries) |
| `info` | Request lifecycle, engine reloads, startup/shutdown |
| `debug` | Detailed processing, SQL queries, connector calls |
| `trace` | Fine-grained internal state |

Every request carries a UUID `x-request-id` header — pass your own or let Orion generate one. The ID propagates through logs and responses for end-to-end correlation.

## Prometheus Metrics

Enable metrics and scrape at `GET /metrics` (Prometheus text format):

```toml
[metrics]
enabled = true
```

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

## Distributed Tracing

Enable OpenTelemetry trace export with OTLP gRPC:

```toml
[tracing]
enabled = true
otlp_endpoint = "http://localhost:4317"
service_name = "orion"
sample_rate = 1.0    # 0.0 (none) to 1.0 (all)
```

- **W3C Trace Context** extraction and propagation — incoming `traceparent` headers are respected
- Per-request spans with channel, workflow, and task attributes
- OTLP gRPC export to Jaeger, Tempo, or any compatible collector
- Configurable sampling rate for production use
- Trace context injected into outbound `http_call` requests for full distributed traces

## Health Monitoring

Orion exposes three health endpoints for different operational needs.

**Component health** — `GET /health` returns component-level status with automatic degradation detection:

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

**Kubernetes probes:**

| Endpoint | Purpose | Behavior |
|----------|---------|----------|
| `GET /healthz` | Liveness probe | Always returns 200 — if the process is running, it's alive |
| `GET /readyz` | Readiness probe | Returns 200 only when DB is reachable, engine is loaded, and startup is complete; 503 otherwise |

```yaml
livenessProbe:
  httpGet:
    path: /healthz
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /readyz
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 5
```

**Engine status** — `GET /api/v1/admin/engine/status` returns a detailed breakdown:

```json
{
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "workflows_count": 42,
  "active_workflows": 38,
  "channels": ["orders", "events", "alerts"]
}
```
