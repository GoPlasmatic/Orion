<!-- description: One-line definitions for every term the Orion documentation uses with a fixed meaning — channel, workflow, connector, deployment, quarantine, rollout and more. -->
<!-- type: glossary -->
<!-- last_verified: 2026-09-14 -->

# Glossary

One definition for every term the book uses with a fixed meaning, ten to twenty words each. A term with a page of its own links to it; the concept page is the canonical home, and this line is the pointer.

**Backpressure**: a per-channel cap on concurrent in-flight work; `max_concurrent_per_node` bounds every ingress together. See [Channel configuration](../reference/channel-config/index.md).

**Case file**: a `*.case.json` regression test that runs a workflow offline and asserts paths in its output. See [Test a workflow offline](../guides/author/testing.md).

**Channel**: a named service endpoint that receives work over HTTP or on a cron schedule, and hands it to a workflow. See [Channels](./channels.md).

**channel_call**: the built-in function that invokes another channel's workflow in-process from a running task. See [Task functions](../reference/functions/channel_call.md).

**Circuit breaker**: a per-`channel:connector`, per-node gate that stops calls to a repeatedly failing backend until a recovery timeout elapses. See [Connectors](./connectors.md).

**Connection**: an immutable revision of a connector's configuration: a provider, public settings and secret locators, named by a deployment. See [Connections](../reference/admin-api/deployments.md#connections).

**Connector**: a named connection to an external system (`http`, `db`, `cache`) that tasks reference by name. See [Connectors](./connectors.md).

**Control plane**: the admin API under `/api/v2/admin/`, and the CLI and Console that drive it. See [Admin API](../reference/admin-api/index.md).

**Cron channel**: a channel whose `protocol` is `cron`: a six-field schedule and a fixed payload, with no route and no caller. See [Cron transport](../reference/channel-config/cron.md).

**Data context**: the JSON document a workflow's tasks read and write; its top level is exactly `data`, `metadata` and `temp_data`. See [Workflows](./workflows.md#the-data-context).

**Data plane**: the endpoints under `/api/v2/data/` where channels answer requests. See [Data API](../reference/data-api.md).

**Dedup key (idempotency key)**: the per-channel value, read from a request header, that marks a request as a duplicate inside the dedup window. See [Channel configuration](../reference/channel-config/deduplication.md).

**Definition**: one deployable workflow, channel or plugin document, stored by the digest of its canonical bytes. See [Definitions](../reference/admin-api/deployments.md#definitions).

**Deployment**: a manifest naming every definition and connection a node serves, committed as an immutable, numbered revision. See [Deployment endpoints](../reference/admin-api/deployments.md).

**Deployment head**: the latest committed deployment revision; every node builds it and serves it. With none, a node serves nothing. See [Convergence](../reference/admin-api/deployments.md#convergence).

**Dialect (portable data dialect)**: the backend-neutral query and write language that `data_query` and `data_write` lower to PostgreSQL, MySQL or SQLite. See [Portable data dialect](../reference/data-dialect.md).

**Engine**: the compiled runtime built from every deployed workflow and plugin; a reload rebuilds and swaps it whole, beside the channel estate. See [Design notes](./design-notes.md#how-hot-reload-swaps-the-runtime-generation).

**Estate**: everything one instance serves: the channels, workflows, connections and plugins of its deployment head.

**Fragment**: a reusable run of tasks a source-form workflow pulls in with `use`, resolved by `compile` before the definition reaches an instance. See [CLI](../reference/cli/shared-definitions.md).

**Generation**: one published value holding the engine, the channel estate, the function registry and the plugin set, swapped atomically on reload. See [Design notes](./design-notes.md#how-hot-reload-swaps-the-runtime-generation).

**Hot reload**: replacing the running generation inside a live process; a committed deployment takes effect without a restart. See [Design notes](./design-notes.md#how-hot-reload-swaps-the-runtime-generation).

**Ingress**: any path work enters a channel: a synchronous request, an `/async` submission, a `channel_call`, or a claimed cron occurrence.

**Ingress guards**: the per-channel checks (rate limit, auth, origin, validation, dedup, response cache, backpressure) that run on every ingress before the workflow. See [Channel configuration](../reference/channel-config/index.md).

**Loop**: a workflow setting that repeats the task list once per sweep, up to a declared `max`, with a counter the tasks can index. See [Workflow definition](../reference/workflows.md#loop).

**Manifest**: a plugin's `plugin.toml`, declaring what the component provides and how it is called. See [Plugins](./plugins.md). A deployment's manifest is its member list. See [Manifests](../reference/admin-api/deployments.md#manifests).

**Modular monolith**: one Orion instance running many services side by side, all from one deployment.

**Occurrence**: one scheduled instant of a cron channel, written to a durable ledger before the work starts and kept after; identified by `(channel_id, scheduled_for)`. See [Cron occurrences](../reference/admin-api/cron-occurrences.md).

**Operation gates**: per-connector booleans under `operations` that permit or refuse `read`, `insert`, `update`, `delete`, `upsert` and `raw_write`. See [Connectors](../reference/connectors/operation-gates.md).

**Plugin**: a definition carrying a WebAssembly component that adds task functions named `<plugin>.<label>`; its world imports nothing. See [Plugins](./plugins.md).

**Quarantine**: the state of a channel that failed to build into a generation; it is refused at every ingress until a later build succeeds. See [Troubleshooting](../operate/maintain/troubleshooting.md).

**Revision**: one immutable, numbered version of a deployment, a manifest member or a connection; a change is always a new revision. See [Deployment endpoints](../reference/admin-api/deployments.md).

**Rollout bucket**: the stable request hash that decides which workflow revision serves a call during a percentage rollout. See [Workflow definition](../reference/workflows.md#rollout).

**Route pattern**: a channel's REST method and path template, with parameters, matched against requests under `/api/v2/data/`. See [Data API](../reference/data-api.md#route-resolution).

**Shaped response**: a response mode in which the workflow sets the HTTP status, headers and body through `data._orion.response`. See [Data API](../reference/data-api.md#shaped-responses).

**Stub file**: canned connector responses, keyed by function and connector name, that let `dry-run` and `test` run a workflow offline. See [Test a workflow offline](../guides/author/testing.md#stub-the-calls-that-leave-the-process).

**Task**: one step of a workflow: an `id`, a `name`, an optional `condition`, and one `function` with its `input`. See [Workflows](./workflows.md#tasks).

**Task group**: an element of `tasks` that carries its own `tasks`: one condition guarding a contiguous run of steps. See [Workflow definition](../reference/workflows.md#task-groups).

**Terminal step**: a task or group with `terminal: true`, which ends the workflow once it has run. See [Workflow definition](../reference/workflows.md#terminal-steps).

**Trace**: the stored record of one channel execution: status, input, result, timings, and optional per-task detail. See [Traces and async processing](../operate/run/traces.md).

**Trace DLQ**: the dead-letter table for async work that could not complete, retried with backoff. See [Admin API](../reference/admin-api/trace-dlq.md).

**Workflow**: a pipeline of tasks selected by JSONLogic conditions, executed by the engine on behalf of a channel. See [Workflows](./workflows.md).
