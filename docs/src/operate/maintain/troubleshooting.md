<!-- description: Orion problems indexed by symptom: what you see, why it happens, what to do, from refused deployments and quarantined channels to tripped breakers. -->
<!-- type: troubleshooting -->
<!-- last_verified: 2026-10-25 -->

# Troubleshooting

Indexed by what you see, most frequent first. Each entry is what it looks like, why it happens, what to do, and how to confirm the fix. The index is collapsed to the symptoms so the whole page fits on one screen.

<div class="fold-sections" data-level="2" data-default="closed" data-skip="related"></div>

## A channel answers `503` "failed to load and is not being served"

**What you see.** Requests to one channel fail with `503` and a message naming it, while every other channel is fine:

```json
{ "error": { "code": "SERVICE_UNAVAILABLE",
             "message": "Channel 'orders' failed to load and is not being served: unknown field `cors`" } }
```

**Why.** The channel is *quarantined*. Its configuration could not be built when the node last built its generation. It was left out of the registry and the route table rather than served with a guard missing. A channel whose `origin_allow_list` did not parse would otherwise serve with no origin check at all, indistinguishable from a channel that deliberately checks nothing.

Quarantine is a load-time failure, not an authentication or authorization outcome. The usual triggers from the channel's own configuration:

- An unknown key in the channel's `config`, including a retired spelling such as `cors` or `backpressure.max_concurrent`.
- A `validation_logic` expression that no longer compiles.
- A credential the config references that does not resolve: an unset `env://` variable in an `auth` block.
- In cluster mode, a dedup or response-cache backend that cannot be built, or one explicitly set to process memory.

And from the workflow behind it, since a channel whose workflow cannot be built has nothing to serve:

- A task naming a function the engine does not dispatch. A typo is the common case; the subtler one is a name that is real but has no handler behind it, such as `enrich`, which Orion does not implement.
- A task whose `input` does not parse into its function's expected shape, or a JSONLogic field on one that does not compile.

`lint`, `compile` and the deployment plan refuse most of these before anything is committed. What reaches quarantine is what differs between nodes: an unset variable, a disabled scheduler, a backend one node cannot build.

**What to do.**

1. Find every affected channel and the reason. The build logged one line per channel (`Channel quarantined: …`), and `/health` lists them:

   ```bash
   curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" \
     http://localhost:8080/health | jq '.channels.quarantined'
   ```

2. If the cause is the node, such as an unset `env://` variable, fix the node and restart it. Its next build serves the channel.
3. If the cause is the definition, fix the file and deploy a new revision: `orion-server compile ./definitions -o dist/deployment`, then `orion-cli deployments apply -d dist/deployment`. A committed revision is never edited. See [Deploy a compiled directory](../../reference/admin-api/deployments.md#deploy-a-compiled-directory).

**How to verify.** `/health` no longer lists the channel under `channels.quarantined`, and `components.channels` reads `ok`. There is no metric for quarantine on the synchronous path. `/health` and the build log are the signals, which is why the `channels` component going `degraded` deserves an alert of its own.

**Traffic meanwhile.** Sync and `/async` HTTP requests get the `503` above. A `channel_call` targeting a quarantined channel fails the calling task the same way.

## My endpoint answers `404` after the apply succeeded

**What you see.** The apply printed a committed revision, but the route still answers `404`:

```json
{ "error": { "code": "NOT_FOUND", "message": "Channel 'orders' not found or not active" } }
```

**Why.** `404` means the path is not a channel this node serves. A channel that exists but failed to load answers `503` instead. Four causes, most likely first:

- The route pattern does not match what you sent. Routes match byte-exactly, including case: `/Orders` does not match `/orders`.
- The head does not carry the channel. A later apply of a directory without it removed it.
- In a cluster, this node has not yet built the head. It catches up within one `cluster.epoch_poll_interval_ms`.
- This node could not build the head and still serves the previous generation. `/health` then reports `engine_reload` as degraded.

**What to do.** Read the head's members and whether this node serves it:

```bash
curl -s http://localhost:8080/api/v2/admin/deployments | jq '{head_revision, applied_revision}'
curl -s http://localhost:8080/api/v2/admin/deployments/<head> | jq '[.manifest.members[].entity]'
curl -s "http://localhost:8080/api/v2/admin/deployments/<head>/convergence?target=observed"
```

If the channel is missing from the members, deploy a directory that carries it. If `applied_revision` trails `head_revision`, see [`/health` reports `engine_reload` as degraded](#health-reports-engine_reload-as-degraded).

**How to verify.** The request answers `200`, or a `4xx` from the workflow's own validation, rather than `404`.

## An apply removed a service I did not touch

**What you see.** The plan or the apply lists members you did not change as removed:

```text
  - workflow order-summary@1
  - channel order-summary@1
```

**Why.** A deployment is everything a node serves. An apply replaces the head with the directory's members, so a service the directory does not carry stops serving. Two teams applying separate directories to one instance remove each other's services.

**What to do.** Keep every service that shares an instance in one definition set, and compile that set. To restore the removed service now, [roll back](../../reference/admin-api/deployments.md#rollback) to the revision before the apply. Read the plan before every apply: `orion-cli deployments plan -d` commits nothing.

**How to verify.** `orion-cli deployments plan -d dist/deployment` lists `0 removed`, and both services answer.

## Plan or apply answers `400 DEPLOYMENT_WOULD_NOT_BUILD`

**What you see.**

```json
{ "error": { "code": "DEPLOYMENT_WOULD_NOT_BUILD",
             "message": "Validation failed: workflow 'x' revision 1: names a channel definition" } }
```

**Why.** Plan and apply build the candidate manifest the way a node would, and refuse one that would not build. Nothing was committed. The message names the cause. Common ones are a member naming a definition of another kind, a workflow calling a function no member serves, and two channels claiming one route.

**What to do.** A directory from `orion-server compile` rarely reaches this, because `compile` runs the same checks offline. Recompile with the binary the target runs, and fix what it reports. A hand-written manifest (`deployments apply -f`) is checked only here, so fix the member the message names.

**How to verify.** `orion-cli deployments plan -d dist/deployment` prints the plan instead of the error.

## Apply answers `409 DEPLOYMENT_HEAD_CONFLICT`

**What you see.**

```json
{ "error": { "code": "DEPLOYMENT_HEAD_CONFLICT",
             "message": "expected base_revision 0, but current head_revision is 1" } }
```

With `orion-cli deployments apply -d --base-revision`, the CLI says `the target's head is revision 1, not 0`.

**Why.** An apply commits only while the head is still the revision it names. Another apply or a rollback committed first. This is the compare-and-swap that stops two pipelines from both winning. `409 REVIEWED_DIGEST_MISMATCH` is its sibling: the manifest differs from the one the plan reviewed.

**What to do.** Read the new head and plan again against it. `orion-cli deployments apply -d` without `--base-revision` plans against whatever the head is now. Check that the other change is one you want to keep, because your apply replaces it.

**How to verify.** The apply answers `202` with the next `committed_revision`.

## `/health` reports `engine_reload` as degraded

**What you see.** `components.engine_reload` is `degraded`, `status` is `degraded` at HTTP 200, and `applied_revision` trails `head_revision`.

**Why.** This node tried to build a committed revision and failed. It keeps serving its previous generation, which is correct but not what is committed. The log has one line per failure: `Deployment build failed: this node is still serving the previous generation`. The usual cause is a node that differs from the one that ran the plan, such as plugins disabled or a different `channel_filter`.

**What to do.** Read the log line's `error`, and fix the node's configuration to match its peers. The reconcile loop retries on every tick, so no reapply is needed once the node can build the head. If the revision itself is wrong, roll back.

**How to verify.** `components.engine_reload` reads `ok`, and the revision's convergence lists this node under `converged_nodes`.

## The admin API answers `400 UNCOMPILED_SOURCE`

**What you see.** Uploading a definition is refused with a field error whose code is `UNCOMPILED_SOURCE`.

**Why.** The document still holds a source-form convenience, such as `$from` or `use`. Those resolve when a definition set is compiled. `POST /api/v2/admin/definitions` takes one document and has no set to resolve the name against.

**What to do.** Deploy the set through `orion-server compile ./definitions -o dist/deployment`, then `orion-cli deployments apply -d dist/deployment`. Do not expand the references by hand.

**How to verify.** The apply uploads every definition without a field error.

## `/health` says `degraded` but returns HTTP 200

**Why.** This is deliberate. A failing *database* is `503` with `"status": "degraded"`. A failed connector load or a quarantined channel is `"status": "degraded"` at HTTP 200. The instance is still serving. A `503` would eject a healthy node from its load balancer over a component nothing in flight may even use.

**What to do.** Point monitors at the `status` field, not only the HTTP code, then read the detail with an admin credential:

```bash
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" http://localhost:8080/health \
  | jq '{status, channels, connectors}'
```

`channels.quarantined` and `connectors.failed_to_load` name the cause. Anonymous callers get only the coarse component states, by design.

**How to verify.** `status` returns to `ok` once the named cause is fixed and the next build succeeds.

## Clients get `429` far below the configured rate

**Why.** The rate limiter identifies callers by TCP peer address. Behind a proxy, load balancer or ingress, that peer is always the proxy, so every client collapses into one bucket.

**What to do.** List the addresses your proxies connect from:

```toml
[rate_limit]
trusted_proxies = ["10.0.0.0/8", "fd00::/8"]
```

Forwarded headers are honoured only when the peer is on that list. Also check the channel's own limit. The default bucket key is per caller, and the platform limiter's budget stacks on top of the channel's rather than being bypassed by it.

**How to verify.** `orion_rate_limit_rejections_total` stops climbing while real request volume is flat; that shape was the signature.

## Everything through one connector answers `503 CIRCUIT_OPEN`

**Why.** That connector's circuit breaker is open. It trips on repeated failures and fails fast instead of piling requests against a backend that is already failing.

**What to do.** Confirm the backend recovered first; a reset against a still-broken backend trips again:

```bash
curl -s http://localhost:8080/api/v2/admin/connectors/circuit-breakers
curl -s -X POST http://localhost:8080/api/v2/admin/connectors/circuit-breakers/{key}
```

Breakers close on their own once calls succeed, so a manual reset is only for cutting the recovery window short. In cluster mode breakers trip per node, so one replica can be failing fast while another still serves. A reset reaches every node through the cluster's reconcile loop. See [Circuit breaker endpoints](../../reference/admin-api/circuit-breakers.md).

**How to verify.** The breaker list shows the key as `closed`, and calls through the connector answer normally.

## The trace DLQ is filling up

**Why.** Async traces that fail land in `trace_dlq` and are retried with exponential backoff. A growing depth means failures are arriving faster than retries succeed, or that retries are off.

**What to do.** Read the errors first; the DLQ is a symptom, not a cause:

```bash
curl -s "http://localhost:8080/api/v2/admin/trace-dlq?limit=20" | jq '.data[].error'
```

Then drain faster by raising `trace_queue.dlq_batch_size`, or purge what is beyond use with `POST /api/v2/admin/trace-dlq/purge` and `{"older_than_hours": 168}` as the body. The age is required, and only exhausted entries are deleted. Check whether retry is even on: with `trace_queue.dlq_retry_enabled = false`, the `orion_trace_dlq_depth` gauge stops updating, so a flat line means "nobody is looking", not "empty".

**How to verify.** `orion_trace_dlq_depth` falls, and stays moving.

## Async submissions never leave `pending`

**Why.** Either the queue is not draining, or the trace row was written and the worker died before finishing.

**What to do.** Check the queue's two bounds first. `buffer_size` caps queued submissions and `max_queue_memory_bytes` caps their total payload; whichever is reached first makes new submissions answer `503`. Then check `orion_trace_dlq_depth` and the worker count (`trace_queue.workers`). A trace stuck at `pending` with nothing in the DLQ usually means the queue is saturated, not that the work failed.

**How to verify.** New submissions move to `completed` within `processing_timeout_ms`.

## A cron channel is deployed but nothing ever runs

**Why.** Almost always `cron.enabled = false` on the node, or on *some* of them. A deployed cron channel on a node with the scheduler off is quarantined rather than ignored, precisely so this is visible. A stored schedule that silently never fires is the one failure an operator cannot see.

**What to do.** Read the component and the quarantine reason:

```bash
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" http://localhost:8080/health \
  | jq '{cron: .components.cron, quarantined: .channels.quarantined}'
orion-cli cron status
```

`components.cron: degraded` means this node has schedules it does not run. In a cluster, check every replica; a mixed setting quarantines the channel on some and runs it on the rest. If the scheduler is on and occurrences are piling up as `pending`, the node is behind rather than off. Compare `cron.workers` against how much work each run does, and look for a `forbid` singleton whose previous occurrence is still running. If they appear as `skipped_misfire`, the schedule's time passed while nothing healthy could start it. That is `misfire_policy` doing its job, and the row carries the count and the range.

**How to verify.** `orion-cli cron list --channel-id <id>` shows new occurrences reaching `completed`.

## A workflow naming a plugin function is quarantined

**Why.** Three causes, distinguishable from the load issue. `plugins.enabled` is `false` on this node, so every deployed plugin becomes a `disabled` load issue. Or the deployment does not carry the plugin. Or the deployed plugin no longer declares that function.

**What to do.** Read the load issues and the catalogue:

```bash
curl -s http://localhost:8080/api/v2/admin/engine/status | jq '.data.load_issues.plugins'
orion-cli functions list | grep '^<plugin>\.'
```

The function catalogue is the authority. A plugin function only appears there once this node serves the plugin. A workflow is validated against the schema that plugin declares.

**How to verify.** The function appears in `orion-cli functions list` with `source: plugin`, and the workflow's channel leaves `channels.quarantined`.

## A condition over the request never fires

**Why.** The raw request payload is not in the JSONLogic context. `{"var": "payload.x"}` resolves to nothing, so a condition written over `payload.*` is false on every request and its task silently skips.

**What to do.** Read the field under `data`, which starts as a copy of the payload: `{"var": "data.x"}`. To keep the request under one name, start the workflow with a `parse_json` task and read `data.req.*`:

```json
{ "id": "parse", "name": "Parse",
  "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } } }
```

**How to verify.** `orion-server dry-run -w workflow.json -i payload.json` prints the context each task produced, starting from the payload's fields under `data`.

## A mapping wrote an object where a value belongs

**Why.** A misspelled JSONLogic operator is not an error. `{"cat": …}` is an operator; `{"catt": …}` is a literal object, and it is written to the target path verbatim. The same applies inside conditions, where the literal is truthy and the condition always fires.

**What to do.** Compare against the operator catalogue in [Expression language](../../reference/expressions.md), and dry-run the workflow before you deploy it.

**How to verify.** The dry-run output shows a value at the path, not an object. A literal object in the output is unmistakable in a trace and invisible in production.

## The server does not start

Startup refusals are deliberate. Each one is a problem that would otherwise be silent:

| Message names | Cause | Fix |
|---|---|---|
| An unknown config key | The key does not exist, or was renamed | The error names the nearest real key. See [Server configuration](../../reference/configuration/index.md) |
| An `ORION_*` variable | A misspelled environment override | Same; Orion refuses rather than ignoring it |
| `rate_limit.trusted_proxies` | A malformed IP or CIDR entry | Fix the entry. This fails even when the limiter is disabled |
| `sqlite:` with cluster mode | Cluster mode needs PostgreSQL or MySQL | Point `storage.url` at a shared database |
| `auto_migrate` in a production cluster | Replicas would race migrations at boot | Set `auto_migrate = false`, run `orion-server migrate` as a deploy step |
| A pending migration | The binary is ahead of the schema | Run `orion-server migrate` |
| Migrations "this binary does not ship" | The state database is an Orion 1.x database | Point `storage.url` at a new database and redeploy. See [Move from Orion 1.x](./upgrades.md#move-from-orion-1x) |
| Missing admin keys | `environment = "production"` with `admin_auth` unset | Supply keys, or do not claim production |

**How to verify.** `orion-server validate-config -c config.toml` reports all of these without starting the server, and `orion-server test-connectivity` proves the database is reachable before the server needs it.

## Related

- [Monitor and alert](../run/monitoring.md): the signals that surface these before a caller does.
- [Traces and async processing](../run/traces.md): the queue and DLQ in full.
- [Timeouts, retries and circuit breakers](../run/failure-handling.md): the controls behind several entries above.
- [Errors and response envelopes](../../reference/errors.md): every error code and what it means.
- [Deployments](../../concepts/deployments.md): the head, the build check and convergence these entries refer to.
