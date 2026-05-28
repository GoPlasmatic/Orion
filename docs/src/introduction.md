<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>The declarative services runtime for the AI era</p>
</div>

**Orion** is a declarative services runtime written in Rust. Instead of writing, deploying, and operating a microservice for every piece of business logic, you **declare** what the service should do, and Orion runs it. Architectural governance — observability, rate limiting, circuit breakers, versioning, input validation, and more — is built in.

AI generates workflows, Orion provides the governance. Every service gets health checks, metrics, retries, and error handling, regardless of how the workflow was created.

Replace a sprawl of single-purpose microservices with one runtime: each channel and workflow is independently versioned, testable, and deployable, but they share a single binary and a single set of built-in production features. This site is the deep reference and how-to guide — new here? [**Install Orion and ship your first service**](./tutorials/cli-setup.md) in a couple of minutes.

## Three Primitives

You build services in Orion with three things:

```
┌─────────────┐       ┌──────────────┐       ┌─────────────┐
│   Channel   │──────▶│   Workflow   │──────▶│  Connector  │
│  (endpoint) │       │   (logic)    │       │  (external) │
└─────────────┘       └──────────────┘       └─────────────┘
```

| Primitive | What it is | Example |
|-----------|-----------|---------|
| **Channel** | A service endpoint: sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | A pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | A named connection to an external system, with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions — all through the admin API. **Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability automatically.

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
