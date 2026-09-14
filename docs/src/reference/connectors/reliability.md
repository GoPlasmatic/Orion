<!-- description: What happens when a connector's backend misbehaves: the http retry loop and its deadline, and the per-channel circuit breaker. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Retries and circuit breakers

The one connector type that retries, the loop it runs, and the breaker that sheds load from a failing dependency.

## Retries

Only `http` connectors retry. A `retry` block on any other type is refused with 400 on create and update — it would otherwise be silently ignored. No other connector type re-drives a failed call: a call that timed out may already have been applied.

The `retry` object accepts exactly two keys; an unknown key is refused:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `max_retries` | integer | no | `3` | Retry attempts after the first request. Values above 16 are refused |
| `retry_delay_ms` | integer | no | `1000` | Delay before the first retry, in milliseconds |

The retry loop behaves as follows:

- **Backoff is exponential.** The delay doubles on each attempt and is capped at 60 seconds.
- **The whole loop shares one deadline**: `timeout_ms × (max_retries + 1)`, measured from the first attempt, backoff included. `timeout_ms` is the [`http_call`](../functions/http_call.md) task's timeout.
- **Idempotent methods only**: GET, PUT, and DELETE retry. POST and PATCH retry only when the connector sets `retry_non_idempotent: true` (default `false`); their budget is otherwise exactly one `timeout_ms`.
- **Retryable errors**: HTTP status ≥ 500, `429`, `408`, status `0` (no response), timeouts, and I/O errors. Everything else fails immediately.

> [!WARNING]
> A timed-out POST may already have been applied, so re-sending it can duplicate the side effect. Enable `retry_non_idempotent` only when the endpoint honors an idempotency key the workflow sets in `headers`.

## Circuit breakers

Circuit breakers shed load from a failing dependency. They are global and off by default: the settings live under `[engine.circuit_breaker]` in the [Configuration Reference](../configuration/engine.md#circuit-breaker), not in connector config.

When enabled, breakers behave as follows:

- **One breaker per `channel:connector` pair, per node.** State is in-process and never shared across a cluster.
- **Every connector-backed task function passes through its breaker**, not only `http_call`.
- **Only retryable failures count.** A call the backend rejected — a syntax error, a constraint violation — says nothing about the dependency's health and never trips the breaker.
- **The breaker opens** after `failure_threshold` consecutive retryable failures. While open, calls fail immediately with `503 CIRCUIT_OPEN` ([error codes](../errors.md)).
- **Half-open admits a single probe.** After `recovery_timeout_secs`, one request is let through. Success closes the breaker; failure reopens it.
- **The breaker map is bounded** at `max_breakers` entries with LRU eviction. Eviction prefers a closed victim: evicting an open breaker would re-admit full load to a dependency still known to be broken. Only when every breaker is open does plain LRU apply, with a warning.

List and reset breakers through the [Admin API](../admin-api/connectors.md).

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [`http`](./http.md): the `retry` and `retry_non_idempotent` fields.
- [Engine settings](../configuration/engine.md#circuit-breaker): where the breaker settings live.
- [`http_call`](../functions/http_call.md): the task timeout the whole loop is measured against.
