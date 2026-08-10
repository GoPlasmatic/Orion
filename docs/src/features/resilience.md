# Resilience

Orion protects your services from cascading failures, transient errors, and overload with circuit breakers, automatic retries, timeouts, and graceful degradation, all built into the runtime.

## Circuit Breakers

Connector calls run through circuit breakers when the global breaker is
enabled (`[engine.circuit_breaker]`, off by default). When failures exceed the
threshold, the breaker opens and short-circuits requests to prevent cascading
failures. Trip conditions, per-`channel:connector` isolation, and eviction are
specified in
[Connector Types › Circuit breakers](../reference/connectors.md#circuit-breakers);
the config fields live in the
[Configuration Reference](../reference/configuration.md).

Inspect and reset breakers via the admin API:

```bash
# List all circuit breaker states
curl -s http://localhost:8080/api/v1/admin/connectors/circuit-breakers

# Reset a specific breaker
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/circuit-breakers/{key}
```

## Retry-Backoff

HTTP connectors retry retryable failures with exponential backoff; no other
connector type is ever re-driven — an operation that timed out may already
have been applied. The full contract (defaults, the idempotent-method rule,
what counts as retryable) is specified in
[Connector Types › Retries](../reference/connectors.md#retries-http-only).

## Timeouts

Timeouts are enforced at multiple levels to prevent runaway requests:

**Per-channel timeout:** a channel's `timeout_ms` bounds workflow execution
on every ingress; `/async` and Kafka ceilings clamp it. Per-ingress defaults
and the clamp table are specified in
[Channel Configuration › Timeouts](../reference/channel-config.md#timeouts);
the reasoning behind the clamp is in
[Design Notes › Kafka's timeout clamp](../reference/design-notes.md#kafkas-timeout-clamp).

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
