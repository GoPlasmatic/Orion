<!-- description: One-line definitions for every term the Orion documentation uses with a fixed meaning — channel, workflow, connector, package, quarantine, rollout and more. -->
# Glossary

One-line definitions for every term the Orion documentation uses with a fixed
meaning.

**Backpressure**: a per-channel cap on concurrent in-flight work; one semaphore
bounds all ingresses together, so `max_concurrent_per_node` limits the
channel's total. See [Channel Configuration](./channel-config.md).

**Channel**: a named service endpoint that receives requests over HTTP or Kafka
and hands them to a workflow. See
[Channel Configuration](./channel-config.md).

**channel_call**: the built-in function that invokes another channel's workflow
in-process from a running task. See
[Functions](./functions.md#channel_call).

**Circuit breaker**: a per-`channel:connector`, per-node gate that stops calls
to a repeatedly failing backend until a recovery timeout elapses. See
[Connectors](./connectors.md).

**Closure (of a package)**: the selected channels, their workflows, and every
connector those workflows reference — what `package export` computes and
ships. See [Promote Between Environments](../operate/promotion.md).

**Config epoch**: a shared counter in the database, advanced by every mutation;
cluster replicas poll it and resync when it moves.

**Connector**: a named, reusable connection definition (`http`, `kafka`, `db`,
`cache`, or `es`) that workflow functions reference by name. See
[Connectors](./connectors.md).

**Data context**: the JSON document a workflow's tasks read and write; its top
level is exactly `data`, `metadata`, and `temp_data`. See
[Workflows](./workflows.md).

**Dedup key (idempotency key)**: the per-channel value — a header, or the Kafka
record key — that marks a request as a duplicate inside the dedup window. See
[Channel Configuration](./channel-config.md).

**Dialect (portable data dialect)**: the backend-neutral query and write
language that `data_query` and `data_write` lower to SQL, MongoDB, or
Elasticsearch. See [Portable Data Dialect](./data-dialect.md).

**Draft / active / archived**: the three entity statuses — drafts are editable,
active versions are immutable and served, archived versions are retired. See
[Admin API](./admin-api.md).

**Engine**: the compiled runtime built from every active channel and workflow;
a reload rebuilds and swaps it as a whole. See
[Design Notes](./design-notes.md).

**Estate**: everything one instance stores — its channels, workflows, and
connectors, across all versions. See [Admin API](./admin-api.md).

**Hot reload**: replacing the engine inside a running process; admin mutations
and `POST /api/v1/admin/engine/reload` take effect without a restart. See
[Admin API](./admin-api.md).

**Ingress**: any path a request enters a channel — a synchronous request, an
`/async` submission, a Kafka record, or a `channel_call`.

**Ingress guards**: the per-channel checks — rate limit, validation, dedup,
response cache, backpressure, timeout — that run on every ingress before the
workflow. See [Channel Configuration](./channel-config.md).

**Modular monolith**: one Orion instance running many independently shipped
services side by side. See [Promote Between Environments](../operate/promotion.md).

**Operation gates**: per-connector booleans under `operations` that permit or
refuse each data operation (`read`, `insert`, `update`, `delete`, `upsert`,
`raw_write`). See [Connectors](./connectors.md).

**Package**: the channels, workflows, and connectors of one service, versioned
as a unit and promoted between instances. See
[Promote Between Environments](../operate/promotion.md).

**Promotion**: moving a package between instances through `export`, `plan`,
and `apply`. See [Promote Between Environments](../operate/promotion.md).

**Quarantine**: the state of a channel that failed to build during an engine
reload; it is not served until the next successful reload.

**Receipt (package receipt)**: the target instance's stored record of a package
application; an applied version is content-immutable. See
[Promote Between Environments](../operate/promotion.md).

**Rollout bucket**: the stable request hash that decides which workflow version
serves a call during a percentage rollout. See
[Workflows](./workflows.md#rollout).

**Route pattern**: a channel's REST method and path template, with parameters,
matched against requests under `/api/v1/data/`. See
[Data API](./data-api.md).

**Shaped response**: a response mode in which the workflow sets the HTTP
status, headers, and body through `data._orion.response`. See
[Data API](./data-api.md#shaped-responses).

**Trace**: the stored record of one channel execution — status, input, result,
timings, and optional per-task detail. See [Data API](./data-api.md).

**Trace DLQ**: the dead-letter table for async work that could not complete —
failed trace persistence, and deliveries to quarantined channels — retried
with backoff. See [Admin API](./admin-api.md#trace-dlq).

**Workflow**: a versioned pipeline of tasks selected by JSONLogic conditions,
executed by the engine on behalf of a channel. See
[Workflows](./workflows.md).

## Related

- [Functions](./functions.md) — every built-in task function with its input
  schema.
- [Configuration](./configuration.md) — the server settings behind these
  behaviours.
- [Admin API](./admin-api.md) — the endpoints that manage the entities defined
  here.
