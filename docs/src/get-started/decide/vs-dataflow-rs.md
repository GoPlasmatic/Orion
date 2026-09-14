<!-- description: Library or runtime: embedding dataflow-rs in your own Rust program versus running Orion, which wraps the same engine in storage, lifecycle, an API and traces. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Orion and embedding dataflow-rs

[dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) is the workflow engine Orion is built on. Embed it when you want task execution *inside* a Rust program you already have. Run Orion when you want that engine as a standing service, with endpoints, connectors, versioning and an admin API around it.

<div class="compare-meta">

**How it relates:** Sits under Orion

**Where they overlap:** the execution model is the same one; Orion does not reimplement it

**Last reviewed:** 2026-08, against dataflow-rs 3.7

</div>

This is the one page here that is not a competitive comparison. It is a build decision: library, or runtime.

## Side by side

|  | dataflow-rs | Orion |
|---|---|---|
| What it is | A Rust crate you add to your own binary | A server you deploy |
| Unit of work | A message processed by an engine you construct | A [channel](../../concepts/channels.md) answering a request or a record |
| How you write the logic | Rust, plus workflow definitions you load yourself | JSON, posted to a running server |
| Where state lives | Wherever your program puts it | The embedded database, or PostgreSQL or MySQL |
| How a change ships | Recompile and redeploy your binary | One API call, hot-reloaded |
| Who serves the endpoint | You do | Orion does |
| What it needs to run | Your application | [One binary](../install.md) |

## What embedding dataflow-rs is good at

- **Custom task functions in Rust, with I/O.** You implement the handler trait and register whatever you like: sockets, files, the clock. Orion's equivalent, a [plugin](../../concepts/plugins.md), is a WebAssembly component that imports nothing, so it can compute but not connect.
- **No server in the picture.** The engine runs inside a process you already operate: a CLI, a batch job, an existing service.
- **Full control of the lifecycle.** You decide where workflow definitions come from, when they reload, and what happens when one fails to build.
- **No opinions about ingress.** There is no channel model, no guard order, and no route table to work within.

## What Orion adds on top

Everything between "the engine can run a pipeline" and "this is a service you can operate":

- [Channels](../../concepts/channels.md): REST routes, plain HTTP, Kafka topics and cron schedules, each with the ingress guards its transport can carry.
- [Connectors](../../concepts/connectors.md): pooled, credential-holding connections with circuit breakers, referenced by name.
- Storage and [lifecycle](../../concepts/lifecycle.md): draft, active, archived, immutable versions, and an atomic engine swap on activation.
- The [admin API](../../reference/admin-api/index.md), the CLI, and the agent skill that lets an assistant drive all of it.
- [Traces](../../operate/run/traces.md), [metrics](../../operate/run/monitoring.md), [packages and promotion](../../operate/maintain/promotion.md), and [cluster mode](../../operate/deploy/cluster.md).

## Where they overlap

The engine is not similar on both sides; it is the same engine. The workflow shape, the [data context](../../reference/workflows.md#the-data-context), condition evaluation and the [`loop`](../../reference/workflows.md#loop) all come from dataflow-rs. What you learn writing one transfers to the other. Orion's [data functions](../../reference/functions/index.md) for parsing, mapping, filtering, validation and logging are the engine's own too.

What does not transfer is everything that touches a connector. Those handlers are Orion's, and most workflows are largely built from them. So a definition moves between the two in the parts the engine owns, and stops at the first `http_call`.

In an estate running both, keep the definitions in Orion and let the embedded engine carry only the step that needs Rust. Splitting them the other way means maintaining the same pipeline in two places, one of which needs a deploy.

## Choose dataflow-rs when

- You are writing Rust and the workflow belongs inside the program.
- You need a task function that does something Orion's set does not, and doing it over HTTP is not acceptable.
- You do not want a server, an admin API, or a database in the picture.
- The workflow definitions are yours to manage, from a source of your choosing.

## Choose Orion when

- You want the endpoint, not only the engine.
- The logic should change without recompiling anything.
- You need versioning, rollout, rollback and an audit trail around changes.
- Someone other than a Rust programmer, including an assistant, authors the logic.

## Running both

They are the same engine. A Rust service that embeds dataflow-rs can call an Orion channel over HTTP for the parts that should stay hot-reloadable. The usual reason is a custom handler: keep the one step that needs Rust in your binary. The logic around it stays in Orion, where it can change without a deploy.

## What Orion cannot do here

- **No custom task functions with I/O.** A dataflow-rs handler can do anything Rust can. An Orion [plugin](../../concepts/plugins.md) is a WebAssembly component that runs in a sandbox importing nothing, so it can compute but not connect. The handler that needs a socket, a file or a clock stays in your binary. See [what you can extend](../../concepts/how-orion-works.md#what-you-can-extend).
- **No control over engine construction.** Orion decides when the engine rebuilds and what goes into it.
- **No embedding.** Orion is a server, not a crate you add to your program.
- **No feature control over the engine's dependencies.** Orion pins dataflow-rs and the operator set that comes with it, so the execution model moves when the engine does. The workflow [`loop`](../../reference/workflows.md#loop) is the example: it arrived with dataflow-rs 3.3, and what Orion added was the guard around it, refusing a `max` over `engine.max_loop_iterations` when you save the workflow rather than failing the engine rebuild later.

## Related

- [Is Orion right for you?](./is-orion-right-for-you.md): the chart, and the other neighbours.
- [How Orion works](../../concepts/how-orion-works.md): the three primitives, and the extension boundary.
- [Task functions](../../reference/functions/index.md): the whole set, and which come from where.
- [Design notes](../../concepts/design-notes.md): the internals behind the guarantees.
