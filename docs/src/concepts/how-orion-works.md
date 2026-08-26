<!-- description: How Orion works: the three primitives, one request's journey through the engine, sync versus async ingress, deployment topology, and the extension surface. -->
# How Orion Works

Orion is a service runtime that executes service definitions you send it over an API. You describe a service as JSON; Orion stores it, validates it, and serves it. There is no build step, no artifact to deploy, and no process to restart.

## Three primitives

You build every service from the same three objects.

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

| Primitive | Description | Examples |
|-----------|-------------|----------|
| **[Channel](./channels.md)** | Service entry point (synchronous HTTP/REST or asynchronous Kafka consumer) | `POST /orders`, `GET /users/{id}`, topic `order.placed` |
| **[Workflow](./workflows.md)** | Sequential task pipeline defining the business logic | Parse input → validate → enrich → transform → return response |
| **[Connector](./connectors.md)** | Reusable client connection to an external data store or service | PostgreSQL, Redis, MongoDB, Elasticsearch, Kafka cluster, REST API |

Channels receive traffic. Workflows process it. Connectors reach outside. Rate limiting, metrics, retries and versioning are the runtime's job, not yours.

The primitives of one service group into a **[package](./packages.md)**: the named, versioned unit that moves between instances. One Orion runs many packages side by side — a modular monolith, where each service ships on its own schedule without becoming its own deployment.

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

1. **Orion finds the channel.** A REST route pattern matches the method and path; if none does, the path is looked up as a channel name.
2. **The channel's guards run.** Whatever that channel declares — rate limit, authentication, origin allow-list, payload validation, deduplication, response cache, backpressure — is enforced before any logic executes. Every ingress gets the same contract, minus what its transport cannot carry. See [Channel Configuration](../reference/channel-config.md).
3. **A workflow is selected.** The channel names one workflow; the engine picks the version to run from its condition and any active rollout percentage.
4. **The tasks run in order.** Each task reads and writes one shared data context. Connector-backed tasks call out; the rest transform data in process.
5. **The context is returned**, as a JSON response for a sync channel or as a stored trace for an async one.

## Sync and async

```
Sync     POST /api/v1/data/{channel}         → immediate JSON response
Async    POST /api/v1/data/{channel}/async   → returns a trace_id for polling
REST     GET  /api/v1/data/orders/{id}       → matched by route pattern
Kafka    topic: order.placed                 → consumed automatically
```

- **Use sync channels for** request/response APIs where the caller waits for the answer.
- **Use async channels for** submissions the caller should not block on. The caller gets a `trace_id` immediately and polls `GET /api/v1/admin/traces/{id}` for the result.
- **Use Kafka channels for** event streams, where the topic is the ingress.

Bridging between them is a pattern, not a feature: a sync workflow can `publish_kafka` and return immediately, and an async channel picks the message up from there.

## Deployment topology

The same binary serves both planes. Only the configuration and the backends change.

| Operational Dimension | Development Environment | Production Environment |
|---|---|---|
| **Control Plane** | Admin API invoked via CLI or Web Console | Admin API managed via automated GitOps / CI-CD pipelines |
| **Data Plane** | Local HTTP testing traffic | Load-balanced HTTP REST and Kafka event traffic |
| **Datastore** | Embedded SQLite (zero external dependencies) | High-availability PostgreSQL or MySQL cluster |
| **Clustering** | Single process (no coordination required) | Multi-replica cluster coordinated via shared database and Redis |
| **Schema Migrations** | Automatically applied at startup | A deploy step; replicas never migrate at boot |
| **Scaling Model** | Single local instance | Horizontally scaled replicas behind a load balancer |
| **Engine Hot-Reload** | Triggers immediately on workflow activation | Distributed across replicas with zero request downtime |

Nothing about the artifacts changes between the two. A workflow authored against SQLite on a laptop is the same JSON that runs on a Postgres-backed fleet, which is what makes [promotion](./packages.md) a file transfer rather than a rewrite.

## What you can extend

Orion is configurable in three places, and closed everywhere else. Being plain about that is more useful than a feature list:

- **Expressions.** Conditions and mappings are [JSONLogic](../reference/expressions.md), evaluated by the engine.
- **Connectors.** New external systems are reached by configuring a connector of a supported type — HTTP, SQL, cache, MongoDB, Elasticsearch, Kafka.
- **Composition.** `channel_call` runs another channel's workflow in-process, so services compose without a network hop.

There is **no plugin mechanism, no scripting runtime, and no WASM sandbox**. A task function you need but Orion does not have is either a `http_call` to a service you write, or a feature request.

## Next steps

- [Channels](./channels.md), [Workflows](./workflows.md) and [Connectors](./connectors.md) — one page per primitive.
- [The Entity Lifecycle](./lifecycle.md) — draft, active, archived, and what each transition does to the running engine.
- [Is Orion Right for You?](../comparison.md) — the same boundary from outside: which neighbouring tool does this job better, and where Orion loses.
- [Install & Run](../getting-started/install.md) then [Your First Service](../getting-started/first-service.md) — the calls that turn the diagram above into a live endpoint.
- [Design Notes](../reference/design-notes.md) — the internals behind the guarantees, for when you want them.
