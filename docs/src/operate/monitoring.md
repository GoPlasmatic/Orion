<!-- description: Structured logs, Prometheus metrics, OpenTelemetry spans and three health endpoints from one Orion binary — and what is actually worth alerting on. -->
# Monitoring & Alerts

Orion emits structured logs, Prometheus metrics, OpenTelemetry spans, and three
health endpoints from the single binary. There are no sidecars or agents to
deploy — you turn each one on and point it somewhere.

## Structured logging

```toml
[logging]
level = "info"      # trace, debug, info, warn, error
format = "json"     # json for production, pretty for a terminal
```

JSON output goes straight into Loki, Datadog, or CloudWatch without a parser.
`RUST_LOG` gives per-crate control when you need it —
`RUST_LOG=orion=debug,tower_http=warn,sqlx=warn` — and overrides the level above
for the crates it names.

| Level | What lands here |
|-------|-----------------|
| `error` | Failures that need attention |
| `warn` | Degraded behaviour: circuit breakers opening, retries, dropped traces |
| `info` | Request lifecycle, engine reloads, startup and shutdown |
| `debug` | Per-connector calls, SQL, detailed processing |
| `trace` | Fine-grained internal state |

Every request carries an `x-request-id`. Send your own or let Orion generate
one; it appears in the logs and comes back on the response, which is how you
join a user's complaint to the lines that describe it.

## Prometheus metrics

```toml
[metrics]
enabled = true
```

Scrape `GET /metrics` in Prometheus text format. With `metrics.enabled = false`
the route is not registered at all, so `/metrics` answers `404` — a deployment
with metrics off cannot be mistaken for a working scrape target that happens to
have no series.

**Give the scraper its own listener.** `/metrics` is otherwise protected by
`admin_auth`, and that same credential can rewrite workflows and read trace
payloads:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"   # unauthenticated; the address is the access control
```

`bind_addr` moves the endpoint onto its own plain-HTTP listener and removes it
from the main one.

Every series carries the `orion_` prefix, and in cluster mode an `instance`
label naming the replica. Histograms export explicit buckets, so
`histogram_quantile()` aggregates correctly across replicas. Names, types, and
labels are in the [Metrics Reference](../reference/metrics.md).

### Where a slow request actually goes

Three histograms nest, and subtracting them attributes latency without turning
on per-request tracing:

```promql
# Engine overhead: condition evaluation, group gating, loop bookkeeping,
# audit writes — everything the engine does that is not a task body.
  sum(rate(orion_workflow_duration_seconds_sum[5m])) by (workflow)
- sum(rate(orion_task_duration_seconds_sum[5m]))     by (workflow)
```

`orion_task_duration_seconds` covers **every** dispatched task, including the
engine's own data functions (`map`, `filter`, `parse_json`, …) that no
connector metric can see. `orion_connector_request_duration_seconds` is the
narrower view of the same work, keyed by connector rather than by task — so
subtracting *it* from the task total separates time spent talking to a backend
from time spent shaping data.

A workflow skipped by its condition or [rollout](../reference/workflows.md#rollout)
gate records nothing, so the count of `orion_workflow_duration_seconds` is
workflow *runs*, not match attempts. A looping workflow records once for the
whole loop rather than once per sweep.

## What to alert on

Five signals are easy to miss because nothing fails loudly when they fire.

- **Background jobs stall silently.** Trace cleanup and DLQ retry swallow
  per-tick errors by design. Alert on
  `time() - orion_job_last_success_timestamp_seconds{job="…"}` exceeding a few
  tick intervals — not on an error rate, which stays at zero.
- **DLQ depth goes stale when retry is off.** `orion_trace_dlq_depth` is
  refreshed by the retry loop. With `trace_queue.dlq_retry_enabled = false` it
  simply stops updating, so a flat line is not an empty queue.
- **Any dropped audit event is a hole in the trail.** Alert on
  `orion_audit_events_dropped_total` existing at all, not on a threshold.
- **Lost trace writes have exactly one signal.**
  `orion_trace_persistence_failures_total` counts writes that will never happen.
  Those rows are gone; nothing retries them later.
- **Kafka throttling is not loss.** A sustained `kafka_guard_deferred` rate in
  `orion_errors_total` means channel guards are throttling the topic. Offsets
  stay uncommitted and records retry, so this shows up as lag rather than
  errors.

Beyond these, alert on what every service needs: error rate by channel, P99
latency, and `/readyz` failures.

## Distributed tracing

```toml
[tracing]
enabled = true
otlp_endpoint = "http://localhost:4317"
service_name = "orion"
sample_rate = 1.0    # 0.0 (none) to 1.0 (all)
```

Spans export over OTLP gRPC to Jaeger, Tempo, or any compatible collector, with
channel, workflow, and task attributes. Incoming `traceparent` headers are
honoured and propagated into outbound `http_call` requests, so an Orion hop does
not break a distributed trace — including through Kafka headers.

> [!NOTE]
> **Two sampling knobs, different jobs.** `tracing.sample_rate` governs OTLP
> span export. Trace *persistence* sampling
> ([`trace_storage.sample_rate`](../reference/configuration.md#trace-persistence))
> applies to **sync traces only** — an async submission's trace row is how its
> result reaches the caller, so async traces always persist. Bound async trace
> storage with `errors_only` or `trace_queue.retention_hours` instead. See
> [Traces & Async Processing](./traces.md).

## Health endpoints

Three endpoints answer three different questions.

| Endpoint | Question | Behaviour |
|----------|----------|-----------|
| `GET /healthz` | Is the process alive? | Always `200` while the process runs. Liveness probe. |
| `GET /readyz` | Should it receive traffic? | `200` only when the database is reachable, startup finished, no required background task has died, and — when enabled — cluster Redis answers and Kafka ingestion is not degraded. Readiness probe. |
| `GET /health` | What is the state of each part? | Component-level status with degradation detail. |

```yaml
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /readyz, port: 8080 }
  initialDelaySeconds: 5
  periodSeconds: 5
```

> [!WARNING]
> **Point monitors at `/health`'s `status` field, not only at its HTTP code.**
> A failing database answers `503` with `"status": "degraded"`. But a failed
> connector load, a quarantined channel, or a dead Kafka consumer also report
> `"status": "degraded"` at HTTP **200** — the instance still serves traffic,
> and a `503` would eject a healthy node from its load balancer over a component
> nothing in flight may even use.

`/health` is deliberately two-tier. Anonymous callers get the coarse component
states; detail fields — `workflows_loaded`, the per-connector circuit-breaker
map, failed connector loads, quarantined channel names — are served only when
admin auth is disabled or the caller presents a valid admin key. A monitor can
therefore see *that* something is degraded without learning *what*.

`components.kafka` appears only when `kafka.enabled` is true. `components.engine`
is a constant `"ok"`, kept for response-shape stability: the engine snapshot
cannot be unavailable once the process serves.

### `components.config_propagation`

Cluster mode only. `degraded` means this node committed a change, applied it
locally, and then failed to advance the shared config epoch — so the other
replicas have not been told. The request that made the change still succeeded,
because it did; see
[Cluster › When a change does not propagate](cluster.md#when-a-change-does-not-propagate)
for why that is a node-health signal rather than a client error. It clears on
the next successful bump. `/readyz` is unaffected: this node is serving
correctly, and ejecting it would not help the ones that are stale.

### `components.background_tasks`

The node's long-lived tasks are supervised, and this is what they report:

| Value | Meaning | Effect on `/readyz` |
|---|---|---|
| `ok` | Every task is running. | ready |
| `degraded` | A task is being restarted after a failure, or a non-essential one has given up. | ready |
| `error` | A task the node cannot work without has stopped for good. | **not ready** |

The essential ones are the trace dispatcher, the trace persistence workers, the
audit writer, the DLQ retry consumer, and — in cluster mode — the epoch
watcher. Each fails silently by nature: a dead persistence worker drops traces
and counts them as queue overflow, a dead audit writer loses the record of
every subsequent admin mutation, a dead epoch watcher leaves the node serving
the configuration it booted with. `error` takes the node out of rotation so the
loss stops rather than continues unobserved.

The retention jobs (trace cleanup, audit-log cleanup) are the non-essential
ones: a node that has stopped expiring old rows still answers every request
correctly, so they show as `degraded` and readiness is unaffected.

With an admin credential, `/health` adds a `background_tasks` array naming each
task, its state, and how many times the supervisor has restarted it. **A
running task with a non-zero restart count is the one worth alerting on** — it
is up now, and it has been failing.

For a running instance's own view, `GET /api/v1/admin/engine/status` returns the
version, uptime, workflow counts, and the channel list.

## Watch it in the console

Everything above surfaces visually in
[the Orion console](../getting-started/console.md) — live request rate, error
rate, latency percentiles, outcomes by channel, and trace drill-downs.

<div class="themed-media">
  <img class="media-dark" src="../images/ui-operations-dark.png" alt="Operations dashboard — request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces for a live Orion instance">
  <img class="media-light" src="../images/ui-operations-light.png" alt="Operations dashboard — request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces for a live Orion instance">
</div>

## Related

- [Metrics Reference](../reference/metrics.md) — every series, its type, and its
  labels.
- [Traces & Async Processing](./traces.md) — trace storage modes, the queue, and
  the DLQ.
- [Troubleshooting](./troubleshooting.md) — what to do when one of these signals
  fires.
- [Configuration Reference](../reference/configuration.md#logging-and-metrics) —
  the keys on this page, with defaults.
