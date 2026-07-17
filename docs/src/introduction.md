<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>Deploy high-performance, governed microservices as JSON workflows—without writing boilerplate.</p>
</div>

**Orion** is an API-first declarative services runtime written in Rust. Instead of writing, containerizing, and operating a new microservice for every piece of business logic, you simply **declare** what the service should do as a JSON workflow, and Orion runs it.

Every workflow is executed with enterprise-grade architectural governance—including observability, rate limiting, circuit breakers, caching, input validation, and versioning—built directly into the runtime, not bolted on. Build workflows yourself or let an AI generate them; either way, they run under the same production-grade guarantees.

### Why Orion?

Developers spend too much time building the same boilerplate for microservices—setting up HTTP servers, configuring database connection pools, writing Prometheus exporters, integrating OpenTelemetry, and coding retry loops or circuit breakers. Orion eliminates this overhead entirely.

* **⚡ Zero Boilerplate:** Go from idea to a live REST/Kafka service in seconds. No Dockerfiles, no CI pipelines, and no server boilerplates.
* **🛡️ Built-in Governance:** Out-of-the-box support for rate limiting, circuit breakers, timeouts, caching, and payload validation.
* **🤖 AI-Native & Safe:** Structured JSON workflows are exceptionally easy for LLMs to generate. Safe rollout pipelines (validation, draft/dry-run, rollout percentage, rollbacks) ensure AI-generated code never breaks production.
* **🦀 Rust Performance:** Built on Tokio and Axum. Achieves **6,000+ requests/sec** per instance with single-digit millisecond latency and a tiny memory footprint.
* **🧩 In-Process Composition:** Call other workflows in-process like functions with zero network round-trip overhead.

This site is the deep reference and how-to guide — new here? [**Install Orion and ship your first service**](./tutorials/cli-setup.md) in a couple of minutes.

<div class="asciinema-player" data-cast="casts/quickstart.cast"></div>
<span class="asciinema-caption">From zero to a live, governed service — business logic as JSON, deployed over plain HTTP. ▶ Click to play.</span>

## Three Primitives

You build services in Orion with three things:

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

| Primitive | What it is | Example |
|-----------|-----------|---------|
| **Channel** | A service endpoint: sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | A pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | A named connection to an external system, with auth and retries | Stripe API, PostgreSQL, MongoDB, Elasticsearch, Redis, Kafka cluster |

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions — all through the admin API. **Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability automatically. See [**Dev & Prod Environments**](./topology/environments.md) for how the same binary serves both planes.

## Start here

- [**CLI Setup**](./tutorials/cli-setup.md) — install Orion and ship your first service in a couple of minutes.
- [**MCP Server Setup**](./tutorials/mcp-setup.md) — give an AI assistant full Orion context so it generates valid workflows.
- [**Use Cases & Patterns**](./tutorials/use-cases.md) — complete, tested examples for classification, transformation, routing, and CI/CD.

## Build workflows

- [**Workflow Reference**](./reference/workflows.md) — the workflow & task JSON schema, conditions, error handling, lifecycle, and rollout.
- [**Function Reference**](./reference/functions.md) — every built-in task function and its exact `input` schema.
- [**Admin API**](./api/admin.md) & [**Data API**](./api/data.md) — the full REST surface for managing and calling services.
- [**Configuration**](./configuration/reference.md) — config file, environment variables, database backends, and deployment.

## How it works

- [**Architecture Overview**](./architecture/overview.md) — channels, workflows, the engine, hot-reload, and the request-processing flow.
- **Production features**, all built in and configurable per channel: [Observability](./features/observability.md), [Resilience](./features/resilience.md), [Security](./features/security.md), [Scalability](./features/scalability.md), [Deployability](./features/deployability.md), [Extensibility](./features/extensibility.md), [Availability](./features/availability.md), and [Maintainability](./features/maintainability.md).
