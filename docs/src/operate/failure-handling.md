# Timeouts, Retries & Circuit Breakers

Every workflow that calls something outside the process inherits that thing's
failure modes. Orion's job is to bound them: cap how long a call may take, retry
what is safe to retry, and stop calling a backend that is already failing.

This page is the operator's view of those three controls, plus what happens when
the process itself goes down.

## Bound how long anything may take

Timeouts apply at four levels, from the outside in.

| Level | Setting | Bounds |
|---|---|---|
| Channel | `timeout_ms` in the channel's `config` | Workflow execution for one request |
| Connector (SQL) | `query_timeout_ms`, `connect_timeout_ms` | One query, one connection attempt |
| HTTP client | `engine.global_http_timeout_secs` | Every `http_call`, as a backstop |
| Health check | `engine.health_check_timeout_secs` | How long `/health` waits on a component |

The channel timeout is the one to set deliberately, because it is the promise
you make to the caller. It applies on **every ingress** — sync, `/async`, Kafka,
and `channel_call` — with per-ingress ceilings that clamp it where the transport
demands. The per-ingress defaults and the clamp table are in
[Channel Configuration › Timeouts](../reference/channel-config.md#timeouts);
why Kafka clamps at all is in
[Design Notes › Kafka's timeout clamp](../reference/design-notes.md#kafkas-timeout-clamp).

```json
{ "config": { "timeout_ms": 5000 } }
```

Set connector timeouts *below* the channel timeout. A query allowed 30 s inside
a channel that gives up at 5 s wastes a database connection for 25 s after
nobody is waiting for the answer.

## Retry only what is safe to retry

> [!IMPORTANT]
> **Only HTTP connectors retry.** No other connector type is ever re-driven. An
> `INSERT` that timed out may already have been applied, and Orion cannot tell
> the difference from the outside — so it does not guess.

HTTP retries use exponential backoff, capped at 60 s, and apply only to
retryable failures on idempotent methods. The full contract — what counts as
retryable, the defaults, and the idempotent-method rule — is
[Connector Types › Retries](../reference/connectors.md#retries-http-only).

For everything else, the retry lives one level up:

- **Async traces** that fail go to the dead letter queue and are retried there.
  See [Traces & Async Processing](./traces.md#drain-the-dead-letter-queue).
- **Kafka records** are not committed on failure, so the consumer redelivers
  them; a `[kafka.dlq]` topic catches what keeps failing.
- **Sync requests** return the error to the caller, who is the only party that
  knows whether retrying is safe.

## Stop calling a failing backend

Circuit breakers are **off by default**. Turn them on when workflows call
external services:

```toml
[engine.circuit_breaker]
enabled = true
```

Once on, failures against a connector trip its breaker; calls then fail fast
with `503 CIRCUIT_OPEN` instead of piling up against a dead backend, and the
breaker closes again on its own when calls start succeeding. Breakers are
isolated per `channel:connector`, so one channel's bad traffic cannot open the
breaker another channel depends on. Trip conditions, isolation, and eviction are
specified in
[Connector Types › Circuit breakers](../reference/connectors.md#circuit-breakers).

Inspect and reset them over the admin API:

```bash
curl -s http://localhost:8080/api/v1/admin/connectors/circuit-breakers
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/circuit-breakers/{key}
```

In cluster mode breakers trip **per node** — each replica learns independently
that a backend is down — while a reset fans out to every node over the config
epoch. That asymmetry is deliberate: tripping is an observation, and each node
observes for itself; resetting is a decision, and you make it once.

## Decide what a failing task does to the request

By default a workflow **halts** on the first task that errors, and the error
goes back to the caller. Set `continue_on_error` on the workflow to collect
errors and keep going:

```json
{
  "status": "ok",
  "data": { "req": { "action": "test-call" } },
  "errors": [
    { "code": "TASK_ERROR", "task_id": "call", "message": "HTTP request failed..." }
  ]
}
```

Note the envelope: `"status": "ok"` with a non-empty `errors` array. A client
that only checks the HTTP code will read that as success. Anything relying on
`continue_on_error` must inspect `errors`. The `filter` function offers finer
control — `on_reject: "halt"` stops the workflow, `on_reject: "skip"` skips only
the current task.

## Shut down without dropping requests

`SIGTERM` and `SIGINT` start a controlled sequence built for load balancers:

1. **`/readyz` flips to `503` immediately** — the balancer pulls the node from
   rotation.
2. **The node keeps accepting and serving** for `server.shutdown_drain_secs`
   (default 30 s), so requests the balancer routes here during its own poll
   interval still succeed.
3. **Accepting stops.** In-flight requests get up to
   `server.shutdown_force_timeout_secs` (default 30 s; `0` = unbounded).
4. Kafka consumers stop, the trace cleanup and DLQ retry jobs stop, the async
   trace queue drains under its own timeout, OpenTelemetry spans flush, and the
   process exits.

```toml
[server]
shutdown_drain_secs = 30
shutdown_force_timeout_secs = 30
```

**Make your orchestrator's kill grace exceed the sum of those two.** Kubernetes
`terminationGracePeriodSeconds`, compose `stop_grace_period`. If the grace is
shorter, the orchestrator kills the process mid-drain and the design above buys
you nothing.

`deploy/ha/rolling-drill.sh` demonstrates the result: traffic through a load
balancer while one node is `SIGTERM`ed, asserting every response was a 2xx.

## What survives a panic

A panic inside a request handler is caught at the outermost middleware layer and
answered as a `500`. The process keeps serving; one request fails instead of
every request failing. This is a backstop, not a feature to rely on — a panic
is a bug, and it is logged as one.

## Related

- [Traces & Async Processing](./traces.md) — the DLQ that catches failed async
  work.
- [Monitoring & Alerts](./monitoring.md) — what to watch so you learn about
  these before your callers do.
- [Troubleshooting](./troubleshooting.md) — symptom-first fixes, including a
  breaker that will not close.
- [Connector Types](../reference/connectors.md) — the normative retry and
  breaker specifications.
