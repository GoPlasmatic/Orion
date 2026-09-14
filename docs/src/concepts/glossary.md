<!-- description: One-line definitions for every term the Orion documentation uses with a fixed meaning — channel, workflow, connector, package, quarantine, rollout and more. -->
<!-- type: glossary -->
<!-- last_verified: 2026-09-14 -->

# Glossary

One definition for every term the book uses with a fixed meaning, ten to twenty words each. A term with a page of its own links to it; the concept page is the canonical home, and this line is the pointer.

**Adapter**: the JSONLogic expression in a model manifest that turns the task's JSON into one input tensor. See [Models](./models.md).

**Admission**: the node-run sequence (fetch, verify, parse, probe) a model version passes before it may be activated. See [Models](./models.md#admission).

**Artifact (package)**: the single JSON document a package travels as, carrying its entities, version and content hash. See [Packages](./packages.md).

**Backpressure**: a per-channel cap on concurrent in-flight work; `max_concurrent_per_node` bounds every ingress together. See [Channel configuration](../reference/channel-config/index.md).

**Case file**: a `*.case.json` regression test that runs a workflow offline and asserts paths in its output. See [Test a workflow offline](../guides/author/testing.md).

**Channel**: a named service endpoint that receives work over HTTP, from a Kafka topic or on a cron schedule, and hands it to a workflow. See [Channels](./channels.md).

**channel_call**: the built-in function that invokes another channel's workflow in-process from a running task. See [Task functions](../reference/functions/channel_call.md).

**Circuit breaker**: a per-`channel:connector`, per-node gate that stops calls to a repeatedly failing backend until a recovery timeout elapses. See [Connectors](./connectors.md).

**Closure (of a package)**: the selected channels, their workflows, and every connector, plugin and model those workflows reference. See [Packages](./packages.md#closure-what-travels-with-a-channel).

**Config epoch**: a shared counter in the database, advanced by every mutation; cluster replicas poll it and resync when it moves. See [Run a cluster](../operate/deploy/cluster.md).

**Connector**: a named connection to an external system (`http`, `db`, `cache`, `es`, `kafka`, `smtp`, `storage`) that tasks reference by name. See [Connectors](./connectors.md).

**Control plane**: the admin API under `/api/v1/admin/`, and the CLI and Console that drive it. See [Admin API](../reference/admin-api/index.md).

**Cron channel**: a channel whose `protocol` is `cron`: a six-field schedule and a fixed payload, with no route, topic or caller. See [Cron transport](../reference/channel-config/cron.md).

**Data context**: the JSON document a workflow's tasks read and write; its top level is exactly `data`, `metadata` and `temp_data`. See [Workflows](./workflows.md#the-data-context).

**Data plane**: the endpoints under `/api/v1/data/` where channels answer requests. See [Data API](../reference/data-api.md).

**Dedup key (idempotency key)**: the per-channel value, a header or the Kafka record key, that marks a request as a duplicate inside the dedup window. See [Channel configuration](../reference/channel-config/deduplication.md).

**Dialect (portable data dialect)**: the backend-neutral query and write language that `data_query` and `data_write` lower to SQL, MongoDB or Elasticsearch. See [Portable data dialect](../reference/data-dialect.md).

**Draft / active / archived**: the three entity statuses: drafts are editable, active versions are immutable and served, archived versions are retired. See [The entity lifecycle](./lifecycle.md).

**Engine**: the compiled runtime built from every active workflow and plugin; a reload rebuilds and swaps it whole, beside the channel estate. See [Design notes](./design-notes.md#how-hot-reload-swaps-the-runtime-generation).

**Estate**: everything one instance stores: its channels, workflows, connectors, plugins and models, across all versions.

**Fragment**: a reusable run of tasks a source-form workflow pulls in with `use`, resolved by `compile` before the definition reaches an instance. See [CLI](../reference/cli/shared-definitions.md).

**Generation**: one published value holding the engine, the channel estate, the function registry, the plugin set and the model set, swapped atomically on reload. See [Design notes](./design-notes.md#how-hot-reload-swaps-the-runtime-generation).

**Hot reload**: replacing the running generation inside a live process; admin mutations and `POST /api/v1/admin/engine/reload` take effect without a restart. See [The entity lifecycle](./lifecycle.md#what-moves-the-engine).

**Ingress**: any path work enters a channel: a synchronous request, an `/async` submission, a Kafka record, a `channel_call`, or a claimed cron occurrence.

**Ingress guards**: the per-channel checks (rate limit, auth, origin, validation, dedup, response cache, backpressure) that run on every ingress before the workflow. See [Channel configuration](../reference/channel-config/index.md).

**Loop**: a workflow setting that repeats the task list once per sweep, up to a declared `max`, with a counter the tasks can index. See [Workflow definition](../reference/workflows.md#loop).

**Manifest**: a plugin's `plugin.toml` or a model's `orion:model` JSON: the declaration of what the artifact provides and how it is called. See [Plugins](./plugins.md) and [Models](./models.md).

**Model**: an ONNX graph held in a bucket by reference, admitted by a node, and run from a task with `model_infer`. See [Models](./models.md).

**Modular monolith**: one Orion instance running many independently shipped services side by side. See [Packages](./packages.md).

**Occurrence**: one scheduled instant of a cron channel, written to a durable ledger before the work starts and kept after; identified by `(channel_id, scheduled_for)`. See [Cron occurrences](../reference/admin-api/cron-occurrences.md).

**Operation gates**: per-connector booleans under `operations` that permit or refuse `read`, `insert`, `update`, `delete`, `upsert` and `raw_write`. See [Connectors](../reference/connectors/operation-gates.md).

**Package**: the channels, workflows, connectors, plugins and models of one service, versioned as a unit and promoted between instances. See [Packages](./packages.md).

**Plugin**: a versioned entity carrying a WebAssembly component that adds task functions named `<plugin>.<label>`; its world imports nothing. See [Plugins](./plugins.md).

**Promotion**: moving a package between instances through `export` or `compile`, `plan` and `apply`. See [Promote between environments](../operate/maintain/promotion.md).

**Quarantine**: the state of a channel that failed to build during a reload; it is refused at every ingress until a later reload succeeds. See [The entity lifecycle](./lifecycle.md#when-a-stored-entity-cannot-be-loaded).

**Receipt (package receipt)**: the target instance's record of a package application; an applied version is content-immutable. See [Packages](./packages.md#receipts-and-immutability).

**Rollout bucket**: the stable request hash that decides which workflow version serves a call during a percentage rollout. See [Workflow definition](../reference/workflows.md#rollout).

**Route pattern**: a channel's REST method and path template, with parameters, matched against requests under `/api/v1/data/`. See [Data API](../reference/data-api.md#route-resolution).

**Shaped response**: a response mode in which the workflow sets the HTTP status, headers and body through `data._orion.response`. See [Data API](../reference/data-api.md#shaped-responses).

**Stub file**: canned connector responses, keyed by function and connector name, that let `dry-run` and `test` run a workflow offline. See [Test a workflow offline](../guides/author/testing.md#stub-the-calls-that-leave-the-process).

**Task**: one step of a workflow: an `id`, a `name`, an optional `condition`, and one `function` with its `input`. See [Workflows](./workflows.md#tasks).

**Task group**: an element of `tasks` that carries its own `tasks`: one condition guarding a contiguous run of steps. See [Workflow definition](../reference/workflows.md#task-groups).

**Terminal step**: a task or group with `terminal: true`, which ends the workflow once it has run. See [Workflow definition](../reference/workflows.md#terminal-steps).

**Trace**: the stored record of one channel execution: status, input, result, timings, and optional per-task detail. See [Traces and async processing](../operate/run/traces.md).

**Trace DLQ**: the dead-letter table for async work that could not complete, retried with backoff. See [Admin API](../reference/admin-api/trace-dlq.md).

**Version**: one immutable row of a channel, workflow, plugin or model under its id; a change is always a new version. See [The entity lifecycle](./lifecycle.md).

**Workflow**: a versioned pipeline of tasks selected by JSONLogic conditions, executed by the engine on behalf of a channel. See [Workflows](./workflows.md).
