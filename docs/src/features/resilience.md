# Resilience

Orion protects your services from cascading failures, transient errors, and overload with circuit breakers, automatic retries, timeouts, and graceful degradation, all built into the runtime.

## Circuit Breakers

Every connector gets automatic circuit breaker protection. When failures exceed a threshold, the breaker opens and short-circuits requests to prevent cascading failures.

| State | Behavior |
|-------|----------|
| **Closed** | Normal operation; requests flow through |
| **Open** | Requests rejected immediately (503) after failure threshold exceeded |
| **Half-Open** | After cooldown, one probe request allowed to test recovery |

The circuit breaker uses a lock-free state machine with per-connector isolation. Configure globally:

```toml
[engine.circuit_breaker]
enabled = true
failure_threshold = 5          # Failures before tripping the breaker
recovery_timeout_secs = 30     # Cooldown before half-open probe
max_breakers = 10000           # Max breaker instances (LRU eviction)
```

Inspect and reset breakers via the admin API:

```bash
# List all circuit breaker states
curl -s http://localhost:8080/api/v1/admin/connectors/circuit-breakers

# Reset a specific breaker
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/circuit-breakers/{key}
```

## Retry-Backoff

All HTTP connectors support automatic retries with exponential backoff, capped at 60 seconds:

```json
{
  "retry": {
    "max_retries": 5,
    "retry_delay_ms": 500
  }
}
```

Delay doubles on each retry: 500ms → 1s → 2s → 4s → ... → capped at 60s.

Retries are configured per HTTP connector, so each external service can have its own retry policy. The mechanism retries only retryable failures — network errors, timeouts, 5xx responses, and 429/408 — and gives up immediately on any other 4xx client error.

Only HTTP connectors have a `retry` block. Database, Elasticsearch, cache and
Kafka calls are never re-driven: an operation that timed out may already have
been applied, so a blind re-send duplicates it. Bound those with their own
timeouts and let the circuit breaker below shed load from a failing dependency.

## Timeouts

Timeouts are enforced at multiple levels to prevent runaway requests:

**Per-channel timeout:** set in the channel's `config_json` to limit workflow execution time:

```json
{
  "timeout_ms": 5000
}
```

If the workflow exceeds this limit, the request returns `504 Gateway Timeout`.

The channel's `timeout_ms` governs **every** ingress: a synchronous request, an `/async` submission, a Kafka record, and an in-process `channel_call` are all bounded by it. Where a channel declares none, each ingress falls back to its own default — no deadline at all on the synchronous path, `trace_queue.processing_timeout_ms` for `/async`, `kafka.processing_timeout_ms` for Kafka, and `engine.default_channel_call_timeout_ms` for `channel_call`. A `channel_call` task may still override with its own `timeout_ms`, which outranks the target channel's.

**On two of those ingresses the channel may only shorten the deadline, never lengthen it.** `kafka.processing_timeout_ms` and `trace_queue.processing_timeout_ms` are not merely defaults: they are ceilings, and the channel value is clamped to them.

| Ingress | Channel declares none | Channel declares more than the transport allows |
|---|---|---|
| synchronous HTTP | runs to completion | honoured — nothing else waits on it |
| `/async` | `trace_queue.processing_timeout_ms` | clamped to `trace_queue.processing_timeout_ms` |
| Kafka | `kafka.processing_timeout_ms` | clamped to `kafka.processing_timeout_ms` |
| `channel_call` | `engine.default_channel_call_timeout_ms` | honoured — the task's own `timeout_ms` outranks it anyway |

The reason is that on those two paths the deadline protects something shared. A Kafka dispatch blocks the consumer's poll loop, so a channel asking for ten minutes on a topic would take the consumer past librdkafka's `max.poll.interval.ms` (300 s by default) and get it evicted from its group mid-record. An `/async` dispatch occupies one of a fixed number of queue workers, so an over-long channel deadline starves every other channel's queued work. `timeout_ms` is channel configuration and is not validated against either ceiling, so the clamp is what keeps a config change from becoming an outage. Raise the transport setting if a channel genuinely needs longer there.

**Per-connector query timeout:** for database connectors:

```json
{
  "query_timeout_ms": 30000,
  "connect_timeout_ms": 5000
}
```

**Global HTTP timeout:** for the shared HTTP client used by `http_call`:

```toml
[engine]
global_http_timeout_secs = 30
```

**Engine lock timeouts:** prevent health checks and reloads from blocking indefinitely:

```toml
[engine]
health_check_timeout_secs = 2
```

## Fault Tolerance

**Graceful shutdown:** Orion handles `SIGTERM` and `SIGINT` with a controlled shutdown sequence:

1. HTTP server stops accepting new connections
2. In-flight requests drain (configurable via `shutdown_drain_secs`, default 30s)
3. Kafka consumer (if enabled) is signaled to stop
4. Trace cleanup task is stopped
5. DLQ retry consumer is stopped
6. Async trace queue drains with timeout
7. OpenTelemetry spans are flushed (if enabled)
8. Process exits

**Dead letter queue:** failed async traces are stored in the `trace_dlq` database table with automatic retry:

```toml
[trace_queue]
dlq_retry_enabled = true
dlq_max_retries = 5
dlq_poll_interval_secs = 30
```

For Kafka, failed messages can also be routed to a configurable DLQ topic:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"
```

**Fault-tolerant pipelines:** set `continue_on_error: true` on a workflow to keep the task pipeline running even if individual tasks fail. Errors are collected in the response rather than halting execution:

```json
{
  "status": "ok",
  "data": { "req": { "action": "test-call" } },
  "errors": [
    { "code": "TASK_ERROR", "task_id": "call", "message": "HTTP request failed..." }
  ]
}
```

**Panic recovery:** the outermost middleware layer (`CatchPanicLayer`) catches panics in any handler, returning a 500 response instead of crashing the process.
