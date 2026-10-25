<!-- description: Bound the failure modes a workflow inherits: per-call timeouts, retries for what is safe to retry, and circuit breakers that stop calling a failing backend. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Timeouts, retries and circuit breakers

Every workflow that calls something outside the process inherits that thing's failure modes, and Orion's job is to bound them. It caps how long a call may take, retries what is safe to retry, and stops calling a backend that is already failing. This guide is the operator's view of those three controls, plus what happens when the process itself goes down.

## Before you start

You need access to the instance's config and to the channel definitions whose timeouts you are setting. The normative contracts are in [Connector types](../../reference/connectors/index.md) and [Channel configuration](../../reference/channel-config/index.md).

## Bound how long anything may take

Timeouts apply at four levels, from the outside in:

| Level | Setting | Bounds |
|---|---|---|
| Channel | `timeout_ms` in the channel's `config` | Workflow execution for one request |
| Connector (SQL) | `query_timeout_ms`, `connect_timeout_ms` | One query, one connection attempt |
| HTTP client | `engine.global_http_timeout_secs` | Every `http_call`, as a backstop |
| Health check | `engine.health_check_timeout_secs` | How long `/health` waits on a component |

The channel timeout is the one to set deliberately, because it is the promise you make to the caller:

```json
{ "config": { "timeout_ms": 5000 } }
```

It applies on every ingress (sync, `/async`, `channel_call` and cron), with a ceiling that clamps it on `/async`. The per-ingress defaults and the clamp table are in [Timeouts](../../reference/channel-config/timeout_ms.md); why `/async` clamps at all is in [The `/async` timeout clamp](../../concepts/design-notes.md#the-async-timeout-clamp).

Set connector timeouts *below* the channel timeout. A query allowed 30 s inside a channel that gives up at 5 s wastes a database connection for 25 s. Nobody is waiting for the answer.

## Retry only what is safe to retry

> [!NOTE]
> Only HTTP connectors retry. No other connector type is ever re-driven. An `INSERT` that timed out may already have been applied, and Orion cannot tell the difference from the outside, so it does not guess.

HTTP retries use exponential backoff, capped at 60 s, and apply only to retryable failures on idempotent methods. The full contract, including what counts as retryable, the defaults and the idempotent-method rule, is [Retries](../../reference/connectors/reliability.md#retries).

For everything else, the retry lives one level up:

- **Async traces** that fail go to the dead-letter queue and are retried there. See [Drain the dead-letter queue](./traces.md#drain-the-dead-letter-queue).
- **Cron occurrences** that fail settle `failed` in the [ledger](../../reference/admin-api/cron-occurrences.md), and `POST /api/v2/admin/cron/occurrences/{id}/retry` attempts the same occurrence again.
- **Sync requests** return the error to the caller, who is the only party that knows whether retrying is safe.

## Stop calling a failing backend

Circuit breakers are off by default. Turn them on when workflows call external services:

```toml
[engine.circuit_breaker]
enabled = true
```

Once on, failures against a connector trip its breaker. Calls then fail fast with `503 CIRCUIT_OPEN` instead of piling up against a dead backend. The breaker closes again on its own when calls start succeeding. Breakers are isolated per `channel:connector`, so one channel's bad traffic cannot open the breaker another channel depends on. Trip conditions, isolation and eviction are specified in [Circuit breakers](../../reference/connectors/reliability.md#circuit-breakers).

Inspect and reset them over the admin API:

```bash
curl -s http://localhost:8080/api/v2/admin/connectors/circuit-breakers
curl -s -X POST http://localhost:8080/api/v2/admin/connectors/circuit-breakers/{key}
```

In cluster mode breakers trip per node. Each replica learns independently that a backend is down, while a reset fans out to every node over the config epoch. That asymmetry is deliberate. Tripping is an observation, and each node observes for itself; resetting is a decision, and you make it once.

## Decide what a failing task does to the request

By default a workflow halts on the first task that errors, meaning a handler error or a `5xx`. The error goes back to the caller. Set `"on_failure": "continue"` on a task to record its error and keep going:

```json
{
  "status": "ok",
  "data": { "req": { "action": "test-call" } },
  "errors": [
    { "code": "IO_ERROR", "task_id": "call", "message": "HTTP request failed..." }
  ]
}
```

Note the envelope: `"status": "ok"` with a non-empty `errors` array. A client that only checks the HTTP code reads that as success. Anything relying on a continued failure must inspect `errors`. A workflow's own `continue_on_error: true` is coarser: a failure ends that workflow without failing the request, and its later tasks do not run. The `filter` function offers finer control: `on_reject: "halt"` stops the workflow, `on_reject: "skip"` skips only the current task.

`on_failure` is not the whole axis. It governs handler errors and `5xx`; a task that records a `4xx` is its `on_rejection`, which carries on by default. That is the default a failing [`validation`](../../reference/functions/validation.md) rule lands in. An assertion written as a gate records its `400` and lets the next task run as if it had passed. [`"on_rejection": "halt"`](../../reference/workflows.md#halting-on-a-rejection) on the task is the key that stops it. `orion-server lint` and `orion-server preflight` both report the unguarded shape as `engine.unguarded_validation`. That is worth running over a stored estate, since nothing about it fails at any gate.

A run that continues records each failure's code at [`metadata._orion_errors`](../../reference/workflows.md#branching-on-a-failure), so a later task can answer differently depending on *why* the step failed. A timeout or dropped connection is worth retrying; a `4xx` from the upstream is not:

```json
{ "id": "queue_for_retry",
  "condition": { "in": [ { "var": "metadata._orion_errors.0.code" },
                         ["TIMEOUT_ERROR", "IO_ERROR"] ] } }
```

Records carry the code, task id and status, never the message, which can embed an upstream URL and response body.

## Shut down without dropping requests

`SIGTERM` and `SIGINT` start a controlled sequence built for load balancers:

1. **`/readyz` flips to `503` at once**, so the balancer pulls the node from rotation.
2. **The node keeps accepting and serving** for `server.shutdown_drain_secs` (default 30 s), so requests the balancer routes here during its own poll interval still succeed.
3. **Accepting stops.** In-flight requests get up to `server.shutdown_force_timeout_secs` (default 30 s; `0` = unbounded).
4. The trace cleanup and DLQ retry jobs stop, the async trace queue drains under its own timeout, OpenTelemetry spans flush, and the process exits.

```toml
[server]
shutdown_drain_secs = 30
shutdown_force_timeout_secs = 30
```

Make your orchestrator's kill grace exceed the sum of those two: Kubernetes `terminationGracePeriodSeconds`, compose `stop_grace_period`. If the grace is shorter, the orchestrator kills the process mid-drain and the design above buys you nothing.

## What survives a panic

A panic inside a request handler is caught at the outermost middleware layer and answered as a `500`. The process keeps serving; one request fails instead of every request failing. This is a backstop, not a feature to rely on. A panic is a bug, and it is logged as one.

## Verify

Prove the drain settings on your own hardware:

```bash
deploy/ha/rolling-drill.sh
```

It drives traffic through a load balancer while one node is `SIGTERM`ed, and asserts every response was a 2xx. For the breakers, `GET /api/v2/admin/connectors/circuit-breakers` lists each key with its state once `engine.circuit_breaker.enabled` is on.

## Next steps

- [Traces and async processing](./traces.md): the DLQ that catches failed async work.
- [Monitor and alert](./monitoring.md): what to watch so you learn about these before your callers do.
- [Troubleshooting](../maintain/troubleshooting.md): symptom-first fixes, including a breaker that does not close.
- [Connector types](../../reference/connectors/index.md): the normative retry and breaker specifications.
