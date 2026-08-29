<!-- description: Every Prometheus series Orion exports, one row per metric: request counters, latency histograms with explicit buckets, and engine, queue and connector gauges. -->
# Metrics Reference

Every Prometheus series Orion exports, one row per metric. All names carry the `orion_` prefix. In cluster mode, every series also carries an `instance` label naming the replica. Histograms export explicit buckets, so `histogram_quantile()` aggregates correctly across replicas.

## Core runtime

| Name | Type | Labels | Description |
|------|------|--------|-------------|
| `orion_build_info` | Gauge | `version`, `git_hash`, `build_timestamp` | Always `1`; identifies the build each replica runs. |
| `orion_jwt_rejections_total` | Counter | `reason` | JWTs refused at channel auth, by typed cause (`expired`, `bad_signature`, `alg_rejected`, …) — the wire answer stays uniform; the dashboard does not have to. |
| `orion_oauth_token_requests_total` | Counter | `connector`, `outcome` | Managed-OAuth2 token-endpoint requests ([connector auth](./connectors.md#managed-oauth2)): `ok`, `rejected` (`invalid_grant` and friends — non-retryable, negative-cached), `transport_error` (retryable). A rising `rejected` on a `refresh_token` connector means a burned seed. |
| `orion_messages_total` | Counter | `channel`, `status` | Messages processed, by outcome: `ok`, `error`, `timeout`, or `duplicate`. A run that finished with task errors is `error` on every transport, including the synchronous one that answers `200` with those errors in its envelope. |
| `orion_message_duration_seconds` | Histogram | `channel` | Message processing latency. |
| `orion_active_workflows` | Gauge | — | Workflows loaded in the engine. |
| `orion_errors_total` | Counter | `reason` | Errors by cause: `engine`, `timeout`, `panic`, `kafka_retry`, and other reason codes. |
| `orion_admin_auth_failures_total` | Counter | `reason` | Rejected admin requests: `missing_or_malformed`, `invalid_key`, `locked_out`, or `read_only_write` (a read-only key attempting a mutation — a 403, not a bad credential). |
| `orion_http_requests_total` | Counter | `method`, `path`, `status` | HTTP requests served. |
| `orion_http_request_duration_seconds` | Histogram | `method`, `path`, `status` | HTTP request latency. |
| `orion_db_query_duration_seconds` | Histogram | `operation` | Database query latency. |
| `orion_db_pool_size` | Gauge | — | Connections in the primary database pool. |
| `orion_db_pool_idle` | Gauge | — | Idle connections in the primary pool. |
| `orion_engine_reloads_total` | Counter | `status` | Engine reload events. |
| `orion_engine_reload_duration_seconds` | Histogram | — | Engine reload latency. |
| `orion_circuit_breaker_trips_total` | Counter | `connector`, `channel` | Circuit breaker trips. |
| `orion_circuit_breaker_rejections_total` | Counter | `connector`, `channel` | Requests rejected by open breakers. |
| `orion_connector_requests_total` | Counter | `connector`, `channel`, `status` | Outbound connector calls, by outcome. |
| `orion_connector_request_duration_seconds` | Histogram | `connector`, `channel` | Outbound connector latency. |
| `orion_task_duration_seconds` | Histogram | `workflow`, `task`, `function` | Per-task latency, including the engine's sync built-in functions. |
| `orion_workflow_duration_seconds` | Histogram | `workflow` | Per-workflow-run latency, task bodies included. Subtract the `orion_task_duration_seconds` sum for the same workflow to get the engine's own overhead: condition evaluation, group gating, loop bookkeeping, audit writes. A workflow skipped by its condition or rollout gate is not recorded; a looping workflow records once for the whole loop. |
| `orion_rate_limit_rejections_total` | Counter | `scope` | Rate-limited requests; `scope` is a channel name or route group. |
| `orion_rate_limit_key_unavailable_total` | Counter | `channel` | Rate-limit refusals where the bucket key could not be computed — a subset of the rejections above. Any non-zero rate is a misconfiguration: the channel's `key_logic` failed to evaluate, or resolved to `null`/empty because it reads a header outside the key context. Alert on it. |
| `orion_response_cache_hits_total` | Counter | `channel` | Response-cache hits. |
| `orion_response_cache_misses_total` | Counter | `channel` | Response-cache misses. |
| `orion_job_last_success_timestamp_seconds` | Gauge | `job` | Unix time of each background job's last successful tick: `trace_cleanup`, `audit_cleanup`, `dlq_retry`, `epoch_watcher`, `kafka_lag`. |

> [!NOTE]
> `orion_errors_total{reason="channel_quarantined"}` fires only on the Kafka and async-queue delivery paths. A synchronous call to a quarantined channel returns an error but increments no counter.

## Trace pipeline

| Name | Type | Labels | Description |
|------|------|--------|-------------|
| `orion_trace_queue_depth` | Gauge | — | Async submissions waiting for a worker. |
| `orion_trace_queue_memory_bytes` | Gauge | — | Approximate memory held by queued payloads. |
| `orion_trace_workers_active` | Gauge | — | Trace workers currently running a job. |
| `orion_trace_workers_total` | Gauge | — | Configured trace worker capacity. |
| `orion_trace_queue_rejected_total` | Counter | `reason` | Submissions refused at the door: `full` or `memory`. Both return `503`. |
| `orion_trace_dropped_total` | Counter | `reason` | Traces not persisted: `overflow`, `sampled_out`, `errors_only`, or `off`. |
| `orion_trace_persistence_queue_depth` | Gauge | — | Trace writes waiting in `async` or `batch` mode. |
| `orion_trace_persistence_batch_size` | Histogram | — | Rows committed per batch flush. |
| `orion_trace_persistence_failures_total` | Counter | — | Trace writes the persistence workers could not complete; these traces are lost. |
| `orion_trace_dlq_depth` | Gauge | — | Rows in the trace DLQ; refreshed by the DLQ retry loop. |
| `orion_trace_dlq_retries_total` | Counter | `outcome` | DLQ entries reaching a terminal state: `retried`, `exhausted`, or `failed`. |

## Audit trail

| Name | Type | Labels | Description |
|------|------|--------|-------------|
| `orion_admin_audit_events_total` | Counter | `action`, `resource_type` | Admin mutations recorded. |
| `orion_audit_queue_depth` | Gauge | — | Audit rows accepted but not yet written. |
| `orion_audit_events_dropped_total` | Counter | `reason` | Admin actions that were not recorded: `queue_full`, `write_failed`, `drain_timeout`, or `writer_stopped`. |

## Kafka ingest

These series exist only when `kafka.enabled = true`.

| Name | Type | Labels | Description |
|------|------|--------|-------------|
| `orion_kafka_consumer_lag_messages` | Gauge | `topic`, `partition` | Consumer lag in messages; polled every `kafka.lag_poll_interval_secs`. |
| `orion_kafka_ingest_degraded` | Gauge | — | `1` while ingestion is down; mirrors the `kafka` component of `/readyz`. |

## Scrape the endpoint

Orion serves metrics at `GET /metrics` in Prometheus text format. The route exists only when `metrics.enabled = true`; otherwise it returns `404`. On the main listener the endpoint sits behind admin auth. Set `metrics.bind_addr` to move it onto a dedicated unauthenticated listener. The settings are in the [Configuration Reference](./configuration.md#logging-and-metrics).

## Related

- [Monitoring & Alerts](../operate/monitoring.md) — enable metrics, structured logging, OTLP tracing, and the health endpoints.
- [Configuration Reference](./configuration.md#logging-and-metrics) — the `[metrics]` and `[logging]` settings, including `bind_addr`.
- [CLI Reference](./cli.md) — `orion-cli metrics` fetches this endpoint from the terminal.
