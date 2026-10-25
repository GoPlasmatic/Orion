<!-- description: How Orion works: the three primitives, how a definition set becomes the deployment a node serves, one request's journey, and the extension surface. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# How Orion works

Orion is a service runtime that serves services you describe as JSON. You write a service as files, compile them into a deployment, and apply it to a node. The node serves exactly what that deployment names, and swaps in each new one without a restart.

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

Channels receive traffic. Workflows process it. Connectors reach outside. Rate limiting, metrics, retries and revision history are the runtime's job, not yours.

## From files to a serving node

The three primitives are files in a *definition set*: one directory per estate, checked into your repository. A node never edits them. It serves a [deployment](./deployments.md), which is a manifest of the definitions and connections it runs, committed as a numbered revision.

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "files", "label": "Definition set", "sublabel": "workflow · channel · connector", "type": "service" },
    { "id": "compile", "label": "orion-server compile", "sublabel": "checks, resolves, writes", "type": "ci" },
    { "id": "apply", "label": "deployments apply", "sublabel": "plan, then commit", "type": "gateway" },
    { "id": "node", "label": "Node", "sublabel": "builds the head", "type": "channel" }
  ],
  "edges": [
    { "from": "files", "to": "compile" }, { "from": "compile", "to": "apply" }, { "from": "apply", "to": "node" }
  ]
}
```

The diagram shows the path every change takes to a node:

1. **You write the files.** A workflow file holds the logic, a channel file the endpoint, a connector file a connection's settings and secret references.
2. **`orion-server compile` checks the set offline.** It runs every `lint` check, resolves shared fragments, and writes a deployment directory. A set that would not serve writes nothing.
3. **`orion-cli deployments apply -d` commits it.** It uploads each definition, plans the manifest against the node's head, and commits it as the next revision. A deployment replaces the head whole.
4. **The node builds the revision.** It compiles a new engine from the manifest and swaps it in. Requests in flight finish on the engine they started with.

With nothing committed, a node serves nothing. A rollback commits an earlier manifest again, as a new revision. The CLI wraps the [deployment endpoints](../reference/admin-api/deployments.md), which the [quickstart](../get-started/quickstart.md) calls with `curl`.

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
3. **A workflow is selected.** The channel requires one workflow. The engine runs it when its condition holds. If the deployment splits traffic between two revisions, the request's rollout bucket picks one.
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

The same binary runs on a laptop and in a fleet. Only the configuration and the backends change.

| Dimension | Development | Production |
|---|---|---|
| **Control plane** | `compile` and `deployments apply -d`, run by hand | The same two commands, run by a CI/CD pipeline |
| **Data plane** | Local HTTP test traffic | Load-balanced HTTP traffic and cron schedules |
| **Datastore** | Embedded SQLite, no external dependency | PostgreSQL or MySQL, highly available |
| **Clustering** | One process, no coordination | Replicas coordinated through the shared database |
| **Schema migrations** | Applied at startup | A deploy step; replicas never migrate at boot |
| **Scaling** | One local instance | Replicas behind a load balancer |
| **New revisions** | Built on the node that applied them, at once | Every replica builds the new head with no request downtime |

Nothing about the definitions changes between the two. A set compiled on a laptop is the same bytes a PostgreSQL-backed fleet serves. Each instance numbers its own revisions, so [deploying to another instance](../reference/admin-api/deployments.md#deploy-a-compiled-directory) is a second apply, not a rewrite.

## What you can extend

Orion is configurable in four places, and closed everywhere else. Being plain about that is more useful than a feature list:

- **Expressions.** Conditions and mappings are [JSONLogic](../reference/expressions.md), evaluated by the engine.
- **Connectors.** A new external system is reached through a connector file over a built-in driver: HTTP, PostgreSQL, MySQL, SQLite, or an in-memory cache.
- **Composition.** `channel_call` runs another channel's workflow in-process, so services compose without a network hop.
- **Plugins.** A pure JSON-to-JSON transformation you already have as code ships as a [plugin](./plugins.md): a WebAssembly component that runs in a sandbox importing nothing. It is deployed like any other definition, and a task calls it like any built-in function.

There is no scripting runtime and no general-purpose plugin API. A plugin cannot open a socket, read a file, tell the time, or reach a connector or a secret, by construction. A task function that has to talk to another system is either an `http_call` to a service you write, or a feature request.

## Next steps

- [Channels](./channels.md), [Workflows](./workflows.md) and [Connectors](./connectors.md): one page per primitive.
- [Deployments](./deployments.md): definitions, connections, the head, plan, apply, rollback and convergence.
- [Build your first service](../get-started/tutorials/first-service.md): the commands that turn the first diagram into a live endpoint.
- [Is Orion right for you?](../get-started/decide/is-orion-right-for-you.md): the same boundary from outside, and where a neighbouring tool does the job better.
- [Design notes](./design-notes.md): the internals behind the guarantees, for when you want them.
