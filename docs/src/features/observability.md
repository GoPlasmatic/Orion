# Observability

Orion provides structured logging, Prometheus metrics, distributed tracing, and health monitoring out of the box. No sidecars, no agents. Everything runs inside the single binary.

All of it surfaces visually in [the Orion console](../getting-started/console.md) — live request rate, error rate, latency percentiles, outcomes by channel, and trace drill-downs:

<div class="themed-media">
  <img class="media-dark" src="../images/ui-operations-dark.png" alt="Operations dashboard — request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces for a live Orion instance">
  <img class="media-light" src="../images/ui-operations-light.png" alt="Operations dashboard — request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces for a live Orion instance">
</div>

## Structured Logging

Orion emits structured logs in JSON or pretty-printed format, configurable at runtime:

```toml
[logging]
level = "info"        # trace, debug, info, warn, error
format = "pretty"     # pretty or json
```

**JSON format** is recommended for production. It integrates directly with log aggregators like Loki, Datadog, or CloudWatch:

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

Every request carries a UUID `x-request-id` header. Pass your own or let Orion generate one. The ID propagates through logs and responses for end-to-end correlation.

## Prometheus Metrics

Enable metrics and scrape at `GET /metrics` (Prometheus text format):

```toml
[metrics]
enabled = true
```

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `orion_build_info` | Gauge | `version`, `git_hash`, `build_timestamp` | Always `1`. Join against it to see which build each replica runs. |
| `orion_messages_total` | Counter | `channel`, `status` | Total messages processed |
| `orion_message_duration_seconds` | Histogram | `channel` | Processing latency |
| `orion_active_workflows` | Gauge | — | Workflows loaded in engine |
| `orion_errors_total` | Counter | `type` | Errors encountered |
| `orion_admin_auth_failures_total` | Counter | `reason` | Rejected admin credentials (`missing_or_malformed`, `invalid_key`, `locked_out`) |
| `orion_http_requests_total` | Counter | `method`, `path`, `status` | HTTP requests served |
| `orion_http_request_duration_seconds` | Histogram | `method`, `path`, `status` | HTTP request latency |
| `orion_db_query_duration_seconds` | Histogram | `operation` | Database query latency |
| `orion_engine_reloads_total` | Counter | `status` | Engine reload events |
| `orion_engine_reload_duration_seconds` | Histogram | — | Engine reload latency |
| `orion_circuit_breaker_trips_total` | Counter | `connector`, `channel` | Circuit breaker trip events |
| `orion_circuit_breaker_rejections_total` | Counter | `connector`, `channel` | Requests rejected by open breakers |
| `orion_channel_executions_total` | Counter | `channel` | Channel invocations |
| `orion_rate_limit_rejections_total` | Counter | `client` | Rate-limited requests |
| `orion_job_last_success_timestamp_seconds` | Gauge | `job` | Unix time of the last fully successful tick of each background job: `trace_cleanup`, `audit_cleanup`, `dlq_retry`, `epoch_watcher` (cluster mode), `kafka_lag` (Kafka enabled). The jobs swallow per-tick errors by design, so alert on `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few tick intervals — that is the signal that cleanup or retry has silently stalled. In cluster mode only the lease-holding node stamps the lease-gated jobs (`trace_cleanup`, `audit_cleanup`, `dlq_retry`); `epoch_watcher` and `kafka_lag` stamp on every node, per `instance`. |

All metrics carry the `orion_` prefix so they cannot collide in a shared
registry, and in cluster mode every series also carries an `instance` label
identifying the replica.

Histograms are exported as real Prometheus histograms with explicit buckets,
so `histogram_quantile()` aggregates correctly **across replicas**. (Without
configured buckets the exporter emits pre-computed per-instance quantiles,
which cannot be aggregated — a latency figure summed over a cluster would be
meaningless.)

## Distributed Tracing

Enable OpenTelemetry trace export with OTLP gRPC:

```toml
[tracing]
enabled = true
otlp_endpoint = "http://localhost:4317"
service_name = "orion"
sample_rate = 1.0    # 0.0 (none) to 1.0 (all)
```

- **W3C Trace Context** extraction and propagation: incoming `traceparent` headers are respected
- Per-request spans with channel, workflow, and task attributes
- OTLP gRPC export to Jaeger, Tempo, or any compatible collector
- Configurable sampling rate for production use
- Trace context injected into outbound `http_call` requests for full distributed traces

> **Two sampling knobs.** `tracing.sample_rate` above governs OTLP span
> export. Trace *persistence* sampling —
> [`trace_storage.sample_rate`](../configuration/reference.md#trace-persistence)
> — applies to **sync traces only**: an async submission's trace row is how
> its result is delivered to the caller, so async traces always persist
> regardless of the sample rate. Bound async trace storage with
> `errors_only` or `trace_queue.retention_hours` instead.

## Health Monitoring

Orion exposes three health endpoints for different operational needs.

**Component health:** `GET /health` returns component-level status with automatic degradation detection:

```json
{
  "status": "ok",
  "version": "0.2.0",
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
| `GET /healthz` | Liveness probe | Always returns 200. If the process is running, it's alive |
| `GET /readyz` | Readiness probe | Returns 200 only when DB is reachable, engine is loaded, startup is complete, and — when enabled — cluster Redis answers and Kafka ingestion is not degraded; 503 otherwise |

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

**Engine status:** `GET /api/v1/admin/engine/status` returns a detailed breakdown:

```json
{
  "data": {
    "version": "1.0.0",
    "uptime_seconds": 3600,
    "workflows_count": 42,
    "active_workflows": 38,
    "channels": ["orders", "events", "alerts"]
  }
}
```
