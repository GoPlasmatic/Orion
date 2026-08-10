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

With `metrics.enabled = false` the route is not registered at all: `/metrics`
returns `404`, so a deployment with metrics off cannot be mistaken for a
working scrape target that simply has no series.

`/metrics` is guarded by `admin_auth` like the rest of the admin plane. Since
that credential can also rewrite workflows and read trace payloads, prefer
giving the scraper a listener of its own:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"    # unauthenticated; the address is the access control
```

`bind_addr` moves the endpoint onto its own plain-HTTP listener and removes it
from the main one. See
[the configuration reference](../reference/configuration.md#logging-and-metrics).

Every exported series — name, type, labels, and label value sets — is
cataloged in the [Metrics Reference](../reference/metrics.md). All metrics
carry the `orion_` prefix, and in cluster mode every series also carries an
`instance` label identifying the replica. Histograms are exported with
explicit buckets, so `histogram_quantile()` aggregates correctly across
replicas.

### What to alert on

<!-- TODO(docs2): this section moves to operate/monitoring.md in Phase 3. -->

- **Background jobs stall silently.** The cleanup and retry jobs swallow
  per-tick errors by design. Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals.
- **DLQ depth goes stale when retry is off.** `orion_trace_dlq_depth` is
  refreshed by the retry loop, so it stops updating if
  `trace_queue.dlq_retry_enabled = false`.
- **Any dropped audit event is a hole in the trail.** Alert on
  `orion_audit_events_dropped_total` existing at all, not on a threshold.
- **Lost trace writes have one signal.**
  `orion_trace_persistence_failures_total` counts writes that will never
  happen; the rows are gone.
- **Kafka throttling is not loss.** A sustained `kafka_guard_deferred` rate in
  `orion_errors_total` means the topic is being throttled by channel guards —
  offsets stay uncommitted and records retry.

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
> [`trace_storage.sample_rate`](../reference/configuration.md#trace-persistence)
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
  "version": "1.0.0",
  "uptime_seconds": 3600,
  "components": {
    "database": "ok",
    "engine": "ok",
    "connectors": "ok",
    "channels": "ok"
  }
}
```

`components.kafka` appears only when `kafka.enabled` is true. The `engine`
component is a constant `"ok"` kept for response-shape stability: the engine
snapshot is lock-free, so it cannot be unavailable once the process serves.

Degradation reports on two levels, and they answer different HTTP codes. A
failing database is `503` with `"status": "degraded"`. A failed connector
load, a quarantined channel or a dead Kafka consumer also report
`"status": "degraded"` — but at HTTP **200**: the instance still serves
traffic, and a 503 would eject the node from its load balancer over a
component that nothing in flight may even use. Point monitors at the
`status` field, not only the HTTP code.

Detail fields — `workflows_loaded`, `git_hash`, `build_timestamp`, the
per-connector circuit-breaker map, failed connector loads and quarantined
channel names — are internal topology, served only when admin auth is
disabled or the caller presents a valid admin key. Anonymous callers get the
coarse component states above, so a monitor can see *that* something is
degraded without learning *what*.

**Kubernetes probes:**

| Endpoint | Purpose | Behavior |
|----------|---------|----------|
| `GET /healthz` | Liveness probe | Always returns 200. If the process is running, it's alive |
| `GET /readyz` | Readiness probe | Returns 200 only when DB is reachable, startup is complete, and — when enabled — cluster Redis answers and Kafka ingestion is not degraded; 503 otherwise |

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
