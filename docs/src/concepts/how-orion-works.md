<!-- description: How Orion works: the three primitives, one request's journey through the engine, sync versus async ingress, deployment topology, and the extension surface. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# How Orion works

Orion is a service runtime that executes service definitions you send it over an API. You describe a service as JSON; Orion stores it, validates it, and serves it. There is no build step, no artifact to deploy, and no process to restart.

## Three primitives

Every Orion service has a channel and a workflow. Add connectors when its logic needs to reach an external system.

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "Channel",   "label": "Channel",   "sublabel": "endpoint", "type": "channel" },
    { "id": "Workflow",  "label": "Workflow",  "sublabel": "logic",    "type": "service" },
    { "id": "Connector", "label": "Connector", "sublabel": "external", "type": "datastore" }
  ],
  "edges": [
    { "from": "Channel",  "to": "Workflow" },
    { "from": "Workflow", "to": "Connector" }
  ]
}
```

| Primitive | What it is | Examples |
|-----------|------------|----------|
| **[Channel](./channels.md)** | The service's entry point: a REST route, a plain HTTP name, or a cron schedule | `POST /orders`, `GET /users/{id}`, `0 15 2 * * *` |
| **[Workflow](./workflows.md)** | The ordered task pipeline that is the business logic | Parse input → validate → enrich → transform → respond |
| **[Connector](./connectors.md)** | A reusable client connection to an external system | PostgreSQL, MySQL, a REST API, an in-memory cache |

Channels receive traffic. Workflows process it. Connectors reach outside. Rate limiting, metrics, retries and versioning are the runtime's job, not yours.

A node serves one [deployment](../reference/admin-api/deployments.md): a manifest of the channels, workflows, connections and plugins it runs. Each deployment is an immutable revision. With none committed, a node serves nothing.

## One request's journey

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "req", "label": "Request", "type": "service" },
    { "id": "resolve", "label": "Route resolution", "sublabel": "pattern → channel", "type": "gateway" },
    { "id": "guards", "label": "Ingress guards", "sublabel": "auth · limits · validation", "type": "ci" },
    { "id": "matcher", "label": "Workflow match", "sublabel": "condition + rollout", "type": "gateway" },
    { "id": "pipeline", "label": "Task pipeline", "sublabel": "ordered execution", "type": "gateway" },
    { "id": "resp", "label": "Response", "type": "channel" }
  ],
  "edges": [
    { "from": "req", "to": "resolve" }, { "from": "resolve", "to": "guards" },
    { "from": "guards", "to": "matcher" }, { "from": "matcher", "to": "pipeline" },
    { "from": "pipeline", "to": "resp" }
  ]
}
```

1. **Orion finds the channel.** A REST route pattern matches the method and path. If none does, the path is looked up as a channel name.
2. **The channel's guards run.** Whatever the channel declares is enforced before any logic executes: rate limit, authentication, origin allow-list, payload validation, deduplication, response cache, backpressure. Every ingress gets the same contract, minus what its transport cannot carry. See [Channel configuration](../reference/channel-config/index.md).
3. **A workflow is selected.** The channel names one workflow. The engine picks the revision to run from its condition and any rollout percentage.
4. **The tasks run in order.** Each task reads and writes one shared data context. Connector-backed tasks call out; the rest transform data in process.
5. **The context is returned.** A sync channel answers with it as JSON; an async channel stores it as a trace.

## Sync, async and scheduled

```text
Sync     POST /api/v2/data/{channel}         → immediate JSON response
Async    POST /api/v2/data/{channel}/async   → returns a trace id for polling
REST     GET  /api/v2/data/orders/{id}       → matched by route pattern
Cron     schedule: 0 15 2 * * *              → runs on its own, with no caller
```

- **Use a sync channel** for a request/response API where the caller waits for the answer.
- **Use an async channel** for a submission the caller should not block on. The caller gets a trace id at once and polls `GET /api/v2/admin/traces/{id}` for the result.
- **Use a [cron channel](../guides/patterns/scheduled-workflows.md)** for work with no caller at all. There is nothing to route to and nothing to authenticate, and every scheduled instant becomes a durable occurrence you can read back.

## Deployment topology

The same binary serves both planes. Only the configuration and the backends change.

| Dimension | Development | Production |
|---|---|---|
| **Control plane** | Admin API, driven by the CLI or the Console | Admin API, driven by a CI/CD pipeline |
| **Data plane** | Local HTTP test traffic | Load-balanced HTTP traffic and cron schedules |
| **Datastore** | Embedded SQLite, no external dependency | PostgreSQL or MySQL, highly available |
| **Clustering** | One process, no coordination | Replicas coordinated through the shared database |
| **Schema migrations** | Applied at startup | A deploy step; replicas never migrate at boot |
| **Scaling** | One local instance | Replicas behind a load balancer |
| **Hot reload** | On each deployment, at once | Every replica builds the new head with no request downtime |

Nothing about the artifacts changes between the two. A workflow authored against SQLite on a laptop is the same JSON that runs on a PostgreSQL-backed fleet. That is what makes [deploying to another instance](../reference/admin-api/deployments.md#deploy-a-compiled-directory) a file transfer rather than a rewrite.

## What you can extend

Orion is configurable in four places, and closed everywhere else. Being plain about that is more useful than a feature list:

- **Expressions.** Conditions and mappings are [JSONLogic](../reference/expressions.md), evaluated by the engine.
- **Connectors.** A new external system is reached by configuring a connector of a supported type: HTTP, SQL, or an in-memory cache.
- **Composition.** `channel_call` runs another channel's workflow in-process, so services compose without a network hop.
- **Plugins.** A pure JSON-to-JSON transformation you already have as code ships as a [plugin](./plugins.md): a WebAssembly component that runs in a sandbox importing nothing. It is deployed like any other definition, and a task calls it like any built-in function.

There is no scripting runtime and no general-purpose plugin API. A plugin cannot open a socket, read a file, tell the time, or reach a connector or a secret, by construction. A task function that has to talk to another system is either an `http_call` to a service you write, or a feature request.

## Next steps

- [Channels](./channels.md), [Workflows](./workflows.md) and [Connectors](./connectors.md): one page per primitive.
- [Deployment endpoints](../reference/admin-api/deployments.md): how definitions reach a node, and how a rollback works.
- [Is Orion right for you?](../get-started/decide/is-orion-right-for-you.md): the same boundary from outside, and where a neighbouring tool does the job better.
- [Build your first service](../get-started/tutorials/first-service.md): the calls that turn the diagram above into a live endpoint.
- [Design notes](./design-notes.md): the internals behind the guarantees, for when you want them.
