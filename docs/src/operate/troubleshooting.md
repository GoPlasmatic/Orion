<!-- description: Orion problems indexed by symptom: what you see, why it happens, what to do — from quarantined channels to empty data contexts and tripped circuit breakers. -->
# Troubleshooting

Indexed by what you see. Each entry is **what it looks like → why it happens →
what to do**, collapsed to the symptom so the whole index fits on one screen.

<div class="fold-sections" data-level="2" data-default="closed" data-skip="related"></div>

## A channel answers `503` "failed to load and is not being served"

**What you see.** Requests to one channel fail with `503` and a message naming
it, while every other channel is fine:

```json
{ "error": { "code": "SERVICE_UNAVAILABLE",
             "message": "Channel 'orders' failed to load and is not being served: unknown field `cors`" } }
```

**Why.** The channel is **quarantined**. Its stored configuration could not be
built at the last engine reload, so it was left out of the registry and the
route table rather than served with a guard missing. A channel whose
`origin_allow_list` did not parse would otherwise serve with no origin check at
all — indistinguishable from a channel that deliberately checks nothing.

Quarantine is a **load-time** failure, not an authentication or authorisation
outcome. The usual triggers, from the channel's own configuration:

- An unknown key in the stored `config`, including a retired spelling such as
  `cors` or `backpressure.max_concurrent`.
- A `validation_logic` expression that no longer compiles.
- A credential the config references that will not resolve — an unset `env://`
  variable in an `auth` block.
- In cluster mode, a dedup or response-cache backend that cannot be built, or
  one explicitly set to process memory.

…and from the workflow behind it, since a channel whose workflow cannot be
built has nothing to serve:

- A channel with no `workflow_id`, or one naming a workflow that is not active
  — archived out from under it, or never activated.
- A task naming a function the engine will not dispatch. A typo is the common
  case; the subtler one is a name that is real but has no handler behind it,
  such as `enrich`, which Orion does not implement.
- A task whose `input` does not parse into its function's expected shape, or a
  JSONLogic field on one that does not compile.
- [Rollout](../reference/workflows.md#rollout) percentages across the active
  versions of one workflow that do not sum to 100. Under, and part of the
  traffic matches no version at all; over, and the later versions are
  unreachable. Either way the whole channel is quarantined rather than serving
  a rollout that silently misroutes.

The workflow-side reasons are all-or-nothing per channel: one unusable version
of a partial rollout quarantines the channel rather than leaving its share of
the traffic blackholed.

**What to do.**

1. **Find every affected channel and the reason.** The reload logged one line
   per channel (`Channel quarantined: …`), and `/health` lists them:

   ```bash
   curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" \
     http://localhost:8080/health | jq '.channels.quarantined'
   ```

2. **Scan for the rest before they bite**: `orion-server preflight` reads the
   stored estate and names every channel and workflow the current rules refuse.
3. **Fix the stored config** through the admin API (create a new version;
   active versions are immutable).
4. **Reload.** Quarantine clears **only** when a later reload builds the channel
   successfully — activating something else, or `POST /api/v1/admin/engine/reload`.
   Nothing retries it in the background.

> [!NOTE]
> There is no metric for quarantine on the synchronous path. `/health` and the
> reload log are the signals, which is why the `channels` component going
> `degraded` deserves an alert of its own.

**What happens to traffic meanwhile.** Sync and `/async` HTTP requests get the
`503` above. **Kafka records for a quarantined channel are routed to the DLQ
rather than dropped**, so they are replayable once you fix the config. A
`channel_call` targeting a quarantined channel fails the calling task the same
way.

## A channel answers `404` "not found or not active"

**Why.** Different failure, similar symptom. `404` means the name is not a
serving channel at all: it was never created, it is still a draft, it was
archived, or the route pattern does not match what you sent. `503` means the
channel exists and failed to load.

**What to do.**

```bash
orion-cli channels list                       # is it there, and is it active?
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" \
  http://localhost:8080/health | jq '.workflows_loaded'
```

If the channel is `active` but the request still `404`s, the route pattern is
the suspect. Routes match **byte-exactly**, including case — `/Orders` does not
match a channel declaring `/orders`.

## `/health` says `degraded` but returns HTTP 200

**Why.** This is deliberate. A failing *database* is `503` with
`"status": "degraded"`. A failed connector load, a quarantined channel, or a
dead Kafka consumer is `"status": "degraded"` at HTTP **200**: the instance is
still serving, and a `503` would eject a healthy node from its load balancer
over a component nothing in flight may even use.

**What to do.** Point monitors at the `status` field, not only the HTTP code,
then read the detail with an admin credential:

```bash
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" http://localhost:8080/health \
  | jq '{status, channels, connectors}'
```

`channels.quarantined` and `connectors.failed_to_load` name the cause.
Anonymous callers get only the coarse component states, by design.

## Clients get `429` far below the configured rate

**Why.** The rate limiter identifies callers by TCP peer address. Behind a
proxy, load balancer, or ingress, that peer is always the proxy, so every
client collapses into one bucket.

**What to do.** List the addresses your proxies connect from:

```toml
[rate_limit]
trusted_proxies = ["10.0.0.0/8", "fd00::/8"]
```

Forwarded headers are honoured only when the peer is on that list. Watch
`orion_rate_limit_rejections_total` climb while real request volume is flat —
that shape is the signature.

Also check the channel's own limit: the default bucket key is **per caller**,
and the platform limiter's budget stacks on top of the channel's rather than
being bypassed by it.

## Everything through one connector answers `503 CIRCUIT_OPEN`

**Why.** That connector's circuit breaker is open. It trips on repeated
failures and fails fast instead of piling requests against a backend that is
already failing.

**What to do.** Confirm the backend recovered first — a reset against a still-broken
backend just trips again:

```bash
curl -s http://localhost:8080/api/v1/admin/connectors/circuit-breakers
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/circuit-breakers/{key}
```

Breakers close on their own once calls succeed, so a manual reset is only for
cutting the recovery window short.

In cluster mode breakers trip **per node**, so one replica can be failing fast
while another still serves. A reset fans out to every node over the config
epoch.

## The trace DLQ is filling up

**Why.** Async traces that fail land in `trace_dlq` and are retried with
exponential backoff. A growing depth means failures are arriving faster than
retries succeed, or that retries are off.

**What to do.**

```bash
curl -s "http://localhost:8080/api/v1/admin/trace-dlq?limit=20" | jq '.data[].error'
```

Read the errors first; the DLQ is a symptom, not a cause. Then:

- **Drain faster** by raising `trace_queue.dlq_batch_size`.
- **Purge what is beyond use:** `POST /api/v1/admin/trace-dlq/purge` with
  `{"older_than_hours": 168}` as the body — the age is required, and only
  exhausted entries are deleted.
- **Check whether retry is even on.** With
  `trace_queue.dlq_retry_enabled = false`, the `orion_trace_dlq_depth` gauge
  stops updating, so a flat line means "nobody is looking", not "empty".

## Kafka lag climbs but nothing errors

**Why.** Channel guards are throttling the topic. When a record is deferred by
a rate limit or backpressure, its offset is **not committed** and the record is
redelivered — throttling, not loss. It shows up as lag, not as errors.

**What to do.** Look for a sustained `kafka_guard_deferred` rate in
`orion_errors_total`, then either raise the channel's `rate_limit` and
`backpressure.max_concurrent_per_node`, or add consumers. Messages are processed
strictly sequentially per consumer — the at-least-once commit contract requires
it, so throughput scales by running more instances in the same consumer group,
not by raising a concurrency knob.

If `/readyz` is failing too, Kafka ingestion is degraded rather than throttled;
check broker reachability with `orion-server test-connectivity`.

## Async submissions never leave `pending`

**Why.** Either the queue is not draining, or the trace row was written and the
worker died before finishing.

**What to do.** Check the queue's two bounds first — `buffer_size` caps queued
submissions and `max_queue_memory_bytes` caps their total payload; whichever is
reached first makes new submissions answer `503`. Then check
`orion_trace_dlq_depth` and the worker count (`trace_queue.workers`). A trace
stuck at `pending` with nothing in the DLQ usually means the queue is saturated,
not that the work failed.

## A workflow runs but the data context is empty

**Why.** The raw request payload is **not** in the JSONLogic context.
`{"var": "payload.x"}` resolves to nothing, and every condition referencing
`data.*` evaluates against an empty object, so tasks silently skip and the
response comes back with nothing in it.

**What to do.** Start the workflow with a `parse_json` task:

```json
{ "id": "parse", "name": "Parse",
  "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } } }
```

Then read request data at `data.req.*`. Confirm with
`orion-server dry-run -w workflow.json -i payload.json`, which prints the
context each task produced.

## A mapping wrote an object where a value belongs

**Why.** A misspelled JSONLogic operator is not an error. `{"cat": …}` is an
operator; `{"catt": …}` is a literal object, and it is written to the target
path verbatim. The same applies inside conditions, where the literal is truthy
and the condition always fires.

**What to do.** Compare against the operator catalogue in
[Expression Language](../reference/expressions.md), and dry-run the workflow
before activating it — a literal object in the output is unmistakable in a trace
and invisible in production.

## The server will not start

Startup refusals are deliberate: each one is a problem that would otherwise be
silent.

| Message names | Cause | Fix |
|---|---|---|
| An unknown config key | The key does not exist, or was renamed | The error names the nearest real key. See [Configuration](../reference/configuration.md) |
| An `ORION_*` variable | A misspelled environment override | Same — Orion refuses rather than ignoring it |
| `rate_limit.trusted_proxies` | A malformed IP or CIDR entry | Fix the entry. This fails even when the limiter is disabled |
| `sqlite:` with cluster mode | Cluster mode needs PostgreSQL or MySQL | Point `storage.url` at a shared database |
| `auto_migrate` in a production cluster | Replicas would race migrations at boot | Set `auto_migrate = false`, run `orion-server migrate` as a deploy step |
| A pending migration | The binary is ahead of the schema | Run `orion-server migrate` |
| Missing admin keys | `environment = "production"` with `admin_auth` unset | Supply keys, or do not claim production |

`orion-server validate-config -c config.toml` reports all of these without
starting the server, and `orion-server test-connectivity` proves the database
and brokers are reachable before you find out the hard way.

## Related

- [Monitoring & Alerts](./monitoring.md): the signals that surface these
  before a caller does.
- [Traces & Async Processing](./traces.md): the queue and DLQ in full.
- [Timeouts, Retries & Circuit Breakers](./failure-handling.md): the controls
  behind several entries above.
- [Errors & Response Envelopes](../reference/errors.md): every error code and
  what it means.
