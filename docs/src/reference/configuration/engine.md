<!-- description: The [engine] settings: channel_call depth and timeout, loop and cache bounds, sticky rollouts, connector load failures, the ops budget and the circuit breaker. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Engine settings

The `[engine]` section: the bounds and defaults of the workflow engine, from `channel_call` depth to the circuit breaker.

## Synopsis

```toml
[engine]
health_check_timeout_secs = 2
max_channel_call_depth = 10
default_channel_call_timeout_ms = 30000
max_loop_iterations = 10000
global_http_timeout_secs = 30
max_pool_cache_entries = 100
cache_cleanup_interval_secs = 60
max_memory_cache_entries = 100000
rollout_sticky_header = ""
fail_on_connector_load_error = false
ops_budget = 0

[engine.circuit_breaker]
enabled = false
failure_threshold = 5
recovery_timeout_secs = 30
max_breakers = 10000
```

## Description

Breakers are keyed per channel and connector, so one noisy channel does not trip a shared connector for everyone else. The state is per node. Every connector-backed task function passes through its breaker, not only `http_call`: `db_read`/`db_write`, `cache_read`/`cache_write`, `mongo_read`/`mongo_write`/`mongo_aggregate`, `publish_kafka` and the portable `data_query`/`data_write` dialect.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `engine.health_check_timeout_secs` | `2` | `ORION_ENGINE__HEALTH_CHECK_TIMEOUT_SECS` | Rarely — it bounds the `/readyz` cluster-Redis `PING` (the engine itself is lock-free and needs no health window). |
| `engine.max_channel_call_depth` | `10` | `ORION_ENGINE__MAX_CHANNEL_CALL_DEPTH` | Lower it to catch accidental recursion between channels sooner. |
| `engine.default_channel_call_timeout_ms` | `30000` | `ORION_ENGINE__DEFAULT_CHANNEL_CALL_TIMEOUT_MS` | Default deadline for `channel_call` when the task sets none. |
| `engine.max_loop_iterations` | `10000` | `ORION_ENGINE__MAX_LOOP_ITERATIONS` | Ceiling on a workflow [`loop`](../workflows.md#loop)'s `max`, refused at write time. Raise it for a workload that genuinely needs more sweeps; `0` removes the ceiling. |
| `engine.global_http_timeout_secs` | `30` | `ORION_ENGINE__GLOBAL_HTTP_TIMEOUT_SECS` | Safety net for every outbound HTTP request; shorter connector or task timeouts still win. |
| `engine.max_pool_cache_entries` | `100` | `ORION_ENGINE__MAX_POOL_CACHE_ENTRIES` | Raise only with more than ~100 distinct external connectors. LRU-evicted. |
| `engine.cache_cleanup_interval_secs` | `60` | `ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS` | Sweep interval for expired in-memory cache entries. |
| `engine.max_memory_cache_entries` | `100000` | `ORION_ENGINE__MAX_MEMORY_CACHE_ENTRIES` | Per-namespace bound; see [`max_memory_cache_entries`](#max_memory_cache_entries). Lower it on a memory-constrained host. `0` removes the bound. |
| `engine.rollout_sticky_header` | `""` | `ORION_ENGINE__ROLLOUT_STICKY_HEADER` | Set to the header that identifies a caller (for example `"x-user-id"`) so canary rollouts are stable per caller; see [`rollout_sticky_header`](#rollout_sticky_header). |
| `engine.fail_on_connector_load_error` | `false` | `ORION_ENGINE__FAIL_ON_CONNECTOR_LOAD_ERROR` | **Set to `true` in production.** Refuse to start when an enabled connector cannot be loaded; see [Connector load failures](#connector-load-failures). |
| `engine.ops_budget` | `0` | `ORION_ENGINE__OPS_BUDGET` | Ceiling on the operations one JSONLogic evaluation may perform, on every engine this node builds; `0` installs none. Set it when expressions come from someone other than the operator, such as a tenant's rules or a competitor's model adapters; see [`ops_budget`](#ops_budget). |

### Connector load failures

An enabled connector whose config cannot be loaded — a missing `env://DB_PASSWORD`, an unparseable `config_json`, an unresolvable secret reference — is skipped. It is then *absent*: every workflow using it returns a 500 at request time, which may be hours after the deploy that broke it.

Three surfaces report this:

- `GET /health` sets `components.connectors` to `degraded` and lists the failures under `connectors.failed_to_load`. The overall status becomes `degraded`, but the HTTP status stays **200**: the rest of the instance is serving, and a 503 would pull the node out of its load balancer over a connector nothing in flight may be using. Alert on the field, not the status code.
- `GET /api/v1/admin/connectors` gives every row a `load_status` of `loaded`, `failed`, or `disabled`, with `load_error` and `load_error_stage` on the failures.
- `engine.fail_on_connector_load_error = true` refuses to start at all, so a bad rollout fails where the orchestrator will catch it. This is startup only — a hot reload never takes a running process down.

### `max_memory_cache_entries`

The setting bounds each in-memory cache **namespace**, with LRU eviction on insert. There is no single shared store. Three kinds of namespace each get their own instance and their own bound. They are the built-in dedup store, the built-in response cache, and every `(purpose, connector)` use of a `backend = "memory"` cache connector. A hot workflow cache therefore cannot evict dedup entries, but the budgets add up. Worst-case resident entries are `max_memory_cache_entries × number of namespaces`: the two built-in stores plus up to three (workflow cache, dedup, response cache) for every memory connector. Size a memory-constrained host from that product, not from the single value. Setting `0` disables the bound, at which point entries written without a TTL are never reclaimed. Only do that when the key set is known to be finite.

### `rollout_sticky_header`

The setting decides how a request is bucketed for canary rollouts. With a header configured, the same caller always lands in the same bucket and therefore on the same workflow version. Empty (the default) falls back to the forwarded client IP. With neither available the bucket is random per request, so a caller can flip between versions mid-session.

### `ops_budget`

The setting bounds one JSONLogic evaluation, deterministically, on every engine this node builds: the serving generation, every reload, and `POST /workflows/{id}/test`. An offline `dry-run` runs unbounded. One operation is one dispatched node, one item an iterator examines, or what an operator charges for the data it moves. The [tensor family](../expressions.md#tensors-tensor) charges per element. Constant-folded subtrees cost nothing. The ceiling is per *evaluation*, not per task or message: a task that evaluates ten expressions gets it ten times. It exists for expressions the operator did not write.

How a refusal surfaces is not uniform. A custom function's template field (`http_call.path`, `crypto.data`, a model adapter) fails the task with `BUDGET_EXCEEDED`, which is not retried. A sync caller gets a `400` carrying the refusal's own text: which operations were charged against which ceiling. A built-in `map` mapping fails its task with status `500` and keeps the reason for the log. That is how the engine reports every mapping failure. A **condition** (workflow, task, group, `filter`) fails closed to `false` and is only logged, because condition evaluation has no error channel. A ceiling low enough to trip an ordinary condition therefore reads as "no workflow matched". The caller gets their own input back with a `200`. Size it from the heaviest legitimate expression in the estate, then leave headroom. The counter runs whether or not a ceiling is set; the cost is one add-and-compare per node.

### Circuit breaker

Fields below configure the global breaker; trip conditions and per-instance
behaviour are specified in
[Connector Types › Circuit breakers](../connectors/reliability.md#circuit-breakers).

Sheds load to a failing dependency. After `failure_threshold` consecutive failures the breaker opens and calls return `503 CIRCUIT_OPEN` immediately, until `recovery_timeout_secs` elapses and a probe is admitted.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `engine.circuit_breaker.enabled` | `false` | `ORION_ENGINE__CIRCUIT_BREAKER__ENABLED` | Enable in production whenever workflows reach anything over the network. |
| `engine.circuit_breaker.failure_threshold` | `5` | `ORION_ENGINE__CIRCUIT_BREAKER__FAILURE_THRESHOLD` | Lower to trip sooner on a flaky dependency; raise to tolerate isolated errors. |
| `engine.circuit_breaker.recovery_timeout_secs` | `30` | `ORION_ENGINE__CIRCUIT_BREAKER__RECOVERY_TIMEOUT_SECS` | How long the breaker stays open before probing. |
| `engine.circuit_breaker.max_breakers` | `10000` | `ORION_ENGINE__CIRCUIT_BREAKER__MAX_BREAKERS` | Rarely — bounds the tracked `channel:connector` pairs before LRU eviction. |

## Related

- [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md): the breaker in operation.
- [Version and roll out changes](../../guides/author/versioning.md): sticky rollouts in practice.
- [Expression language](../expressions.md): what `ops_budget` prices.
- [Server configuration](./index.md): every section, by what you are configuring.
