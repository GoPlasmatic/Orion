<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>Turn business logic into APIs your AI can write and your ops team can trust.</p>
  <p class="hero-sub">No new service to build. No deploy to wait for.</p>
</div>

Every piece of business logic tends to turn into its own microservice. You set up a repo, an HTTP server, a Dockerfile, a CI pipeline, metrics, retries, and a deployment, all before you get to the logic itself.

**Orion** works the other way around. You write the logic as JSON, either by hand or by asking an AI, and send it to a running Orion server. A second later it's a live API.

Everything you'd normally build around it is already running: rate limits, retries, caching, metrics, tracing, input validation, versioning, and rollback. Change the logic and the endpoint changes with it. No rebuild, no restart, no downtime.

Orion is a single Rust binary. It stores your service definitions in an embedded database and runs them on Tokio and Axum at 5,000+ measured requests per second. There's nothing to containerize and nothing to provision.

### Why Orion?

Open a small internal microservice and count the lines. HTTP server setup, connection pools, a Prometheus exporter, OpenTelemetry wiring, retry loops, a circuit breaker, health checks, a Dockerfile, a deploy manifest. Somewhere in the middle sits the logic you actually cared about, and it's maybe fifty lines long. Orion runs that middle part for you and provides everything around it, the same way, for every service.

* **⚡ No service to build:** Idea to live REST or Kafka endpoint in seconds. No Dockerfile, no CI pipeline, no server code.
* **🛡️ Production features included:** Rate limiting, circuit breakers, timeouts, caching, and payload validation on every endpoint. You configure them instead of writing them.
* **🤖 Safe for AI-written logic:** Models generate JSON reliably. Validation, draft-before-activate, dry-run, percentage rollout, and one-call rollback mean AI output can't quietly break production.
* **🦀 Rust speed:** Built on Tokio and Axum. **5,100–5,700 requests/sec** per instance (measured, v1.0.0), single-digit millisecond latency, small memory footprint.
* **🧩 Services that call services:** `channel_call` runs another workflow in-process, so there's no network hop and no serialization cost.

This site is the deep reference and how-to guide. New here? [**Install Orion and ship your first service**](./getting-started/install.md) in a couple of minutes.

<div class="themed-media">
  <video class="media-dark" controls muted playsinline preload="metadata" src="videos/ui-quickstart-dark.webm"></video>
  <video class="media-light" controls muted playsinline preload="metadata" src="videos/ui-quickstart-light.webm"></video>
</div>
<span class="asciinema-caption">▶ Zero to a live service in under a minute, without writing code, in <a href="getting-started/console.html">the Orion console</a>. Prefer a terminal? The same flow over plain HTTP:</span>

<div class="asciinema-player" data-cast="casts/quickstart.cast"></div>
<span class="asciinema-caption">From zero to a live, governed service, with business logic as JSON deployed over plain HTTP. ▶ Click to play.</span>

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

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, and manage versions, all through the admin API. **Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability automatically. See [**Dev & Prod Environments**](./topology/environments.md) for how the same binary serves both planes.

The channels, workflows, and connectors that make up one service form a **package** — the unit that exports from one instance and imports into another, versioned and tracked. One Orion runs many packages side by side: a modular monolith, where each service ships independently without becoming its own deployment. See [**Packages & Promotion**](./topology/packages.md).

## Start here

- [**CLI Setup**](./getting-started/install.md): install Orion and ship your first service in a couple of minutes.
- [**MCP Server Setup**](./ai/mcp-setup.md): give an AI assistant full Orion context so it generates valid workflows.
- [**Use Cases & Patterns**](./tutorials/use-cases.md): complete, tested examples for classification, transformation, routing, and CI/CD.

## Build workflows

- [**Workflow Reference**](./reference/workflows.md): the workflow & task JSON schema, conditions, error handling, lifecycle, and rollout.
- [**Function Reference**](./reference/functions.md): every built-in task function and its exact `input` schema.
- [**Admin API**](./reference/admin-api.md) & [**Data API**](./reference/data-api.md): the full REST surface for managing and calling services.
- [**Configuration**](./reference/configuration.md): config file, environment variables, database backends, and deployment.

## How it works

- [**Architecture Overview**](./architecture/overview.md): channels, workflows, the engine, hot-reload, and the request-processing flow.
- **Production features**, all built in and configurable per channel: [Observability](./features/observability.md), [Resilience](./features/resilience.md), [Security](./features/security.md), [Scalability](./features/scalability.md), [Deployability](./features/deployability.md), [Extensibility](./features/extensibility.md), [Availability](./features/availability.md), and [Maintainability](./features/maintainability.md).
