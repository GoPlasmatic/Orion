<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **Deploy high-performance, governed microservices as JSON workflows—without writing boilerplate.**

  [![CI](https://github.com/GoPlasmatic/Orion/actions/workflows/ci.yml/badge.svg)](https://github.com/GoPlasmatic/Orion/actions/workflows/ci.yml)
  [![Crates.io](https://img.shields.io/crates/v/orion-server.svg)](https://crates.io/crates/orion-server)
  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org)
  [![Docs](https://img.shields.io/badge/docs-mdBook-blue.svg)](https://goplasmatic.github.io/Orion/)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion)](https://github.com/GoPlasmatic/Orion/releases)
  [![GitHub Stars](https://img.shields.io/github/stars/GoPlasmatic/Orion?style=social)](https://github.com/GoPlasmatic/Orion)
</div>

Orion is an API-first declarative services runtime written in Rust. Instead of writing, containerizing, and operating a new microservice for every piece of business logic, you simply **declare** what the service should do as a JSON workflow, and Orion runs it. 

Every workflow is executed with enterprise-grade architectural governance—including observability, rate limiting, circuit breakers, caching, input validation, and versioning—built directly into the runtime, not bolted on. Build workflows yourself or let an AI generate them; either way, they run under the same production-grade guarantees.

**Jump to:** [Quickstart](#your-first-service-in-2-minutes) · [Why Orion?](#why-orion) · [Is Orion right for you?](#is-orion-right-for-you) · [Three primitives](#three-primitives) · [What's built in](#whats-built-in) · [Connectors](#connect-to-anything) · [Functions](#built-in-task-functions) · [Performance](#performance) · [Install](#install) · [Docs](#documentation)

---

## Why Orion?

Developers spend too much time building the same boilerplate for microservices—setting up HTTP servers, configuring database connection pools, writing Prometheus exporters, integrating OpenTelemetry, and coding retry loops or circuit breakers. Orion eliminates this overhead entirely.

* **⚡ Zero Boilerplate:** Go from idea to a live REST/Kafka service in seconds. No Dockerfiles, no CI pipelines, and no server boilerplates.
* **🛡️ Built-in Governance:** Out-of-the-box support for rate limiting, circuit breakers, timeouts, caching, and payload validation.
* **🤖 AI-Native & Safe:** Structured JSON workflows are exceptionally easy for LLMs to generate. Safe rollout pipelines (validation, draft/dry-run, rollout percentage, rollbacks) ensure AI-generated code never breaks production.
* **🦀 Rust Performance:** Built on Tokio and Axum. Achieves **6,000+ requests/sec** per instance with single-digit millisecond latency and a tiny memory footprint.
* **🧩 In-Process Composition:** Call other workflows in-process like functions with zero network round-trip overhead.

---

## Your First Service in 2 Minutes

No code. No Dockerfile. No CI pipeline. Just a running service.

<div align="center">
  <img src="media/quickstart.gif" alt="Define a workflow and channel over HTTP, then send a request and get a governed response — all in under a minute" width="100%">
  <br>
  <em>Business logic as JSON, deployed over plain HTTP — flagged in milliseconds, with rate limiting, metrics and tracing already on.</em>
</div>

**1. Start Orion**

```bash
brew install GoPlasmatic/tap/orion-server   # or: curl installer, cargo install (see Install)
orion-server
```

**2. Deploy your first service — one command**

```bash
curl -fsSL https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh | bash
```

The script talks to the same admin API you'd use in production: it creates a **workflow** (the logic — flag any order over $10,000 for review) and a **channel** (the endpoint — `POST /orders`), activates both, and sends a first test order. Re-running it is safe. Cloned the repo? Run `./examples/quickstart.sh` instead.

<details>
<summary><b>What the script does — the four API calls, spelled out</b></summary>

Create the workflow (business logic as JSON — a parse task, then a conditional flag task):

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_id": "high-value-order",
    "name": "High-Value Order",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse payload", "function": {
          "name": "parse_json",
          "input": { "source": "payload", "target": "order" }
      }},
      { "id": "flag", "name": "Flag order",
        "condition": { ">": [{ "var": "data.order.total" }, 10000] },
        "function": {
          "name": "map",
          "input": { "mappings": [
            { "path": "data.order.flagged", "logic": true },
            { "path": "data.order.alert", "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
          ]}
      }}
    ]
  }'

# Activate it (draft → active; the engine hot-reloads)
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

Create the channel — the endpoint that routes to the workflow — and activate it:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "orders", "name": "orders", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/orders",
        "methods": ["POST"], "workflow_id": "high-value-order" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/orders/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

</details>

**3. Call it — your service is live**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

```json
{
  "status": "ok",
  "data": {
    "order": {
      "order_id": "ORD-9182",
      "total": 25000,
      "flagged": true,
      "alert": "High-value order: $25000"
    }
  }
}
```

That's it. The business logic is a JSON document, deploying it was an API call, and rate limiting, metrics, health checks, and request tracing were already active when it went live. Change the threshold? One API call. No rebuild, no redeploy, no restart.

> **Prefer to describe the service instead of writing it?** Workflow JSON is exceptionally easy for LLMs to generate — tell your AI assistant *"flag orders over $10,000 for manual review with an alert message"* and deploy what it returns. [AI Writes Services, Not Code](#ai-writes-services-not-code) shows the safe path from prompt to production.

---

## Is Orion Right for You?

| If you need to... | Orion? | Why |
|---|:-:|---|
| Turn business rules into live REST/Kafka services | **Yes** | Define logic as JSON workflows, deploy with one API call |
| Let AI generate and manage business logic | **Yes** | Built-in validation, dry-run testing, and draft-before-activate safety |
| Replace a handful of single-purpose microservices | **Yes** | One instance handles many channels, governance included |
| Use a rule engine like Drools | **Not quite** | Orion uses [JSONLogic](https://jsonlogic.com) via [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) for conditions and transforms. Lightweight and AI-friendly, but not a full RETE-based rule engine with complex fact networks |
| Embed a workflow engine library in your app | **No** | Orion is a standalone runtime, not a library. For an embeddable workflow engine, see [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) which Orion is built on |
| Build a visual drag-and-drop workflow UI | No | Orion is API-first; pair it with your own UI or use the [CLI](https://github.com/GoPlasmatic/Orion-cli) |
| Orchestrate long-running jobs (hours/days) | No | Use Temporal or Airflow. Orion is optimized for request-response and event processing |
| Run a full API gateway with plugin ecosystem | No | Use Kong or Envoy. Orion focuses on service logic, not proxy features |
| General-purpose compute (image processing, ML) | No | Orion's task functions operate on JSON data. Use custom services or serverless for arbitrary compute |
| Stateful workflows with human-in-the-loop approvals | No | Use [Temporal](https://temporal.io) or BPMN engines. Orion workflows are stateless request pipelines |

Longer, tool-by-tool discussion (Temporal, Kong, Drools, n8n, dataflow-rs): [Is Orion Right for You?](https://goplasmatic.github.io/Orion/comparison.html)

---

## Three Primitives

You build services in Orion with three things:

```mermaid
graph LR
    C["Channel<br>(endpoint)"] --> W["Workflow<br>(logic)"]
    W --> Co["Connector<br>(external)"]

    style C fill:#21252b,stroke:#61afef,stroke-width:2px,color:#abb2bf
    style W fill:#21252b,stroke:#61afef,stroke-width:2px,color:#abb2bf
    style Co fill:#21252b,stroke:#61afef,stroke-width:2px,color:#abb2bf
```

| Primitive | What it is | Example |
|-----------|-----------|---------|
| **Channel** | A service endpoint: sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | A pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | A named connection to an external system, with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions, all through the admin API.

**Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability automatically.

---

## AI Writes Services, Not Code

When AI generates a microservice, you still need to add health checks, metrics, retries, and error handling. When AI generates an Orion workflow, **all of that is already there**. The platform guarantees it.

**Use the [Orion CLI's MCP server](https://github.com/GoPlasmatic/Orion-cli)** to give your AI assistant full Orion context. No manual prompt engineering needed. The MCP server exposes 46 tools covering the full Orion API — workflow syntax, available functions, connector types, and API operations. One config block and you're done (Claude Code `.mcp.json`, Claude Desktop, or any MCP client):

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion-cli",
      "args": ["mcp", "serve"],
      "env": { "ORION_SERVER_URL": "http://localhost:8080" }
    }
  }
}
```

No MCP client? Paste the [**prompt pack**](https://goplasmatic.github.io/Orion/getting-started/prompt-pack.html) — a self-contained context block with Orion's schemas, conventions, and API calls — into any LLM and it can write and deploy workflows through the plain REST API.

```
You: "Classify orders into VIP (>=500, 15% discount), Premium (100-500, 5%), and Standard tiers"

AI:  → generates valid workflow JSON
     → creates it via the API
     → tests with dry-run
     → activates when you approve
```

**Safe path from AI output to production — every time:**

```mermaid
graph TD
    A["1. Generate<br>(AI Workflow JSON via MCP)"] --> B["2. Validate<br>(Verify JSON Syntax & Schema)"]
    B --> C["3. Create Draft<br>(Saved in DB, Offline)"]
    C --> D["4. Dry-Run Test<br>(Verify with Sample Data)"]
    D --> E["5. Activate<br>(Engine hot-reloads)"]
    E --> F["6. Canary Rollout<br>(10% → 50% → 100% traffic)"]

    style A fill:#21252b,stroke:#5c6370,color:#abb2bf
    style B fill:#21252b,stroke:#5c6370,color:#abb2bf
    style C fill:#21252b,stroke:#5c6370,color:#abb2bf
    style D fill:#21252b,stroke:#5c6370,color:#abb2bf
    style E fill:#2e3f2f,stroke:#98c379,stroke-width:2px,color:#98c379
    style F fill:#293c4e,stroke:#61afef,stroke-width:2px,color:#61afef
```

Every AI-generated workflow gets version history, draft-before-activate, dry-run testing, rollout control, structured `FieldError` validation feedback, and audit trails — the same governance as hand-written ones. Roll back to any previous version instantly.

Need to ship a bundle of workflows, channels, or connectors at once (e.g. promoting from staging)? Use the bulk import endpoints — `POST /api/v1/admin/{workflows,channels,connectors}/import?dry_run=true` validates the whole batch first; drop `dry_run` to commit.

See [Use Cases & Patterns](https://goplasmatic.github.io/Orion/tutorials/use-cases.html#ai-workflow--cicd) for CI/CD integration and GitHub Actions examples.

---

## Before & After

**Before** — every piece of business logic is its own service to build, deploy, and operate:

```mermaid
graph TD
    Client["API Clients"] --> GW["API Gateway"]
    GW --> PS["Pricing Service"] --> DB[("Database")]
    GW --> FS["Fraud Service"] --> RD[("Redis Cache")]
    GW --> RS["Routing Service"] --> KF[("Kafka Cluster")]
    GW --> NS["Notification Service"] --> SM["SMTP Server"]

    style Client fill:#21252b,stroke:#5c6370,color:#abb2bf
    style GW fill:#e06c75,stroke:#e06c75,color:#1e222b
    style PS fill:#21252b,stroke:#5c6370,color:#abb2bf
    style FS fill:#21252b,stroke:#5c6370,color:#abb2bf
    style RS fill:#21252b,stroke:#5c6370,color:#abb2bf
    style NS fill:#21252b,stroke:#5c6370,color:#abb2bf
```

**After** — one Orion instance replaces all four (with API gateway and logic engine consolidated, routing traffic, executing workflows, and automatically handling governance):

```mermaid
graph TD
    Client["API Clients"] --> Orion["Orion Server Runtime"]
    
    subgraph Orion ["Orion Runtime (Single Binary)"]
        direction TB
        C1["/pricing (Workflow)"]
        C2["/fraud (Workflow)"]
        C3["/routing (Workflow)"]
        C4["/notify (Workflow)"]
        Gov["Built-in: Rate Limiting, Metrics, Tracing, Circuit Breakers"]
    end
    
    C1 --> DB[("Database")]
    C2 --> RD[("Redis Cache")]
    C3 --> KF[("Kafka Cluster")]
    C4 --> SM["SMTP Server"]

    style Client fill:#21252b,stroke:#5c6370,color:#abb2bf
    style Orion fill:#282c34,stroke:#abb2bf,stroke-width:2px,color:#abb2bf
    style C1 fill:#61afef,stroke:#61afef,color:#1e222b
    style C2 fill:#61afef,stroke:#61afef,color:#1e222b
    style C3 fill:#61afef,stroke:#61afef,color:#1e222b
    style C4 fill:#61afef,stroke:#61afef,color:#1e222b
    style Gov fill:#e5c07b,stroke:#e5c07b,color:#1e222b
```

**The best of both worlds:** each channel and workflow is independently versioned, testable, and deployable. The modularity of microservices with the operational simplicity of a monolith. Change one workflow without touching the others. Roll back a single channel without redeploying everything.

---

## What's Built In

Every channel gets production-grade features without writing a line of code. Configure per channel or use platform defaults:

| Feature | What it does | Configuration |
|---------|-------------|---------------|
| **Rate limiting** | Throttle requests per client or globally | `requests_per_second`, `burst`, JSONLogic key computation |
| **Timeouts** | Cancel slow workflows, return 504 | `timeout_ms` per channel |
| **Input validation** | Reject bad requests at the boundary | JSONLogic with access to headers, query params, path params |
| **Backpressure** | Shed load when overwhelmed, return 503 | `max_concurrent` (semaphore-based) |
| **CORS** | Control browser cross-origin access | `allowed_origins` per channel |
| **Circuit breakers** | Stop cascading failures to external services | Automatic per connector, admin API to inspect/reset |
| **Versioning** | Draft → active → archived lifecycle | Automatic version history, rollout percentages, instant rollback |
| **Observability** | Prometheus metrics, structured logs, distributed tracing | Always on, zero configuration |
| **Health checks** | Component-level status with degradation detection | `GET /health`, automatic |
| **Request IDs** | UUID propagated through the entire pipeline | `x-request-id` header, automatic |
| **Deduplication** | Prevent duplicate processing via idempotency keys | `Idempotency-Key` header, configurable retention window |
| **Response caching** | Cache responses for identical requests | TTL-based, configurable cache key fields |
| **Per-request profiling** | Break a single request down by phase (engine lock, workflow run, tasks) | Opt in with `X-Orion-Profile: 1` or `?profile=1`; surfaces under `_orion.profile` |
| **Per-task tracing** | Capture each task's input/output for replay and debugging | Channel-level `config.tracing.per_task = true`; stored on the trace row |

A minimal channel needs only a name and a workflow. Everything else has sensible defaults.

> **Observability deep dive:** health endpoints, full Prometheus metrics list, Kubernetes probes, and OpenTelemetry tracing config. See [Observability Guide](https://goplasmatic.github.io/Orion/features/observability.html).

---

## Sync and Async

```
Sync     POST /api/v1/data/{channel}         → immediate response
Async    POST /api/v1/data/{channel}/async   → returns trace_id, poll later

REST     GET /api/v1/data/orders/{id}        → matched by route pattern
Kafka    topic: order.placed                 → consumed automatically
```

Sync channels respond immediately. Async channels return a trace ID; poll `GET /api/v1/data/traces/{id}` for results. Kafka channels consume from topics configured in the DB or config file, no restart needed when you add new ones.

**Bridging is a pattern, not a feature.** A sync workflow can `publish_kafka` and return 202. An async channel picks it up from there.

REST channels support parameterized route patterns (`/orders/{order_id}`) with path, query, and header injection into the workflow context. See [Data API](https://goplasmatic.github.io/Orion/api/data.html#route-resolution).

## Service Composition

Most platforms require HTTP calls between services, adding latency, failure modes, and serialization overhead. Orion's `channel_call` invokes another channel's workflow **in-process** with zero network round-trip:

```mermaid
graph TD
    Req["POST /orders (Workflow)"] --> P["parse_json<br>(Extract order data)"]
    P --> C1["channel_call<br>(inventory-check)"]
    C1 --> C2["channel_call<br>(customer-lookup)"]
    C2 --> M["map<br>(Compute pricing)"]
    M --> Res["publish_json<br>(Combined response)"]

    classDef task fill:#21252b,stroke:#5c6370,color:#abb2bf;
    classDef inprocess fill:#61afef,stroke:#61afef,color:#1e222b;
    class P,M,Res task;
    class C1,C2 inprocess;
```

Each composed channel has its own workflow, versioning, and governance, but calls between them are function calls, not network hops. Cycle detection prevents infinite recursion.

---

## Connect to Anything

Connectors are named, reusable connections to external systems. Configure once, reference by name in any workflow. Credentials stay out of your logic:

| Connector type | Systems | Features |
|---------------|---------|----------|
| **HTTP** | Any REST API, webhook, or service | Bearer / Basic / API key auth, retry with backoff, SSRF protection |
| **Database** | PostgreSQL, MySQL, SQLite | Parameterized queries, connection pooling, read + write operations |
| **Cache** | In-memory (built-in) or Redis | TTL-based expiry, also powers deduplication and response caching |
| **MongoDB** | Any MongoDB instance | Document queries, BSON-to-JSON conversion, connection pooling |
| **Elasticsearch** | Any Elasticsearch cluster | Portable `data_query`/`data_write` rendered to Query DSL and `_bulk`, via the shared HTTP client |
| **Kafka** | Any Kafka cluster | Publish with key/value logic, consume with DLQ routing |

Every connector gets **circuit breaker protection** automatically: failures trip the breaker, subsequent calls fast-fail, and the breaker auto-recovers. Database and Elasticsearch connectors also carry **per-operation gates** (`operations: { read, insert, update, delete, upsert, raw_write }`) — set `"delete": false` and no workflow can delete through that connector, no matter what its tasks say. Secrets are stored in the database and masked in API responses, and any string field can use an `env://VAR_NAME` reference to pull the value from the process environment at startup so production credentials never sit in the saved config. See [Connectors Guide](https://goplasmatic.github.io/Orion/features/extensibility.html#connectors) for configuration examples and auth options.

---

## Built-in Task Functions

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context for downstream tasks |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields, constraints, and schema-like checks |
| `http_call` | Invoke downstream APIs, webhooks, or services via [connectors](https://goplasmatic.github.io/Orion/features/extensibility.html#connectors) |
| `channel_call` | Invoke another channel's workflow in-process |
| `data_query` | Portable, backend-neutral read — filter, project, sort, paginate, include related records — runs unchanged on SQL, MongoDB, or Elasticsearch |
| `data_write` | Portable insert/update/delete/upsert using the same envelope across SQL, MongoDB, and Elasticsearch |
| `db_read` | Execute raw SQL SELECT queries, return rows as JSON (escape hatch for CTEs, aggregations, hand-tuned SQL) |
| `db_write` | Execute raw SQL INSERT/UPDATE/DELETE, return affected count |
| `cache_read` | Read from in-memory or Redis cache |
| `cache_write` | Write to cache with optional TTL |
| `mongo_read` | Query MongoDB collections, BSON-to-JSON conversion |
| `publish_json` | Serialize data to JSON output format |
| `publish_xml` | Serialize data to XML output format |
| `publish_kafka` | Publish messages to [Kafka topics](https://goplasmatic.github.io/Orion/features/extensibility.html#kafka-connector) |
| `log` | Emit structured log entries for auditing and debugging |

All functions are built into every binary. The dataflow-rs runtime contributes `parse_json`/`parse_xml`/`filter`/`map`/`validation`/`publish_json`/`publish_xml`/`log`; Orion adds the connector-backed handlers (`http_call`, `data_query`, `data_write`, `db_read`, `db_write`, `cache_read`, `cache_write`, `mongo_read`, `publish_kafka`) and the in-process `channel_call`. `data_query`/`data_write` speak the [portable data dialect](https://goplasmatic.github.io/Orion/reference/data-dialect.html) — write the query once, switch backends by switching connectors. `cache_read`/`cache_write` use the in-memory backend by default; reference a Redis connector for distributed caching. See the [Function Reference](https://goplasmatic.github.io/Orion/reference/functions.html) for every function's exact `input` schema, or browse them at runtime via `GET /api/v1/admin/functions`.

---

## When Things Go Wrong

Production services fail. Orion handles it so you don't write retry loops and fallback logic:

| Failure | What Orion does | You configure |
|---------|----------------|---------------|
| **External API down** | Circuit breaker trips, fast-fails subsequent calls, auto-recovers | `failure_threshold`, `recovery_timeout_secs` per connector |
| **Slow workflow** | Timeout fires, returns 504 to caller | `timeout_ms` per channel |
| **Traffic spike** | Rate limiter rejects excess requests (429), backpressure sheds load (503) | `requests_per_second`, `max_concurrent` per channel |
| **Async task fails** | Moved to Dead Letter Queue, retried automatically with backoff | `dlq_max_retries`, `dlq_poll_interval_secs` |
| **Task in pipeline fails** | Pipeline halts with error, or continues collecting errors if `continue_on_error: true` | Per-workflow setting |
| **Duplicate request** | Detected via idempotency key, returns 409 | `Idempotency-Key` header + retention window |

**Debugging is built in.** Every request gets a `x-request-id` propagated through the entire pipeline. Structured JSON logs show what data each task received and produced. Enable OpenTelemetry for distributed tracing across `http_call` and `channel_call` chains. Inspect circuit breakers, DLQ traces, and debug endpoints via the [API Reference](https://goplasmatic.github.io/Orion/api/admin.html).

---

## Deploy Anywhere

```mermaid
flowchart LR
    subgraph Standalone ["Standalone"]
        direction TB
        S["./orion-server"]
    end

    subgraph Sidecar ["Sidecar Pattern"]
        direction LR
        App["App"] <--> O["Orion"]
    end

    subgraph Container ["Docker / Kubernetes"]
        direction TB
        D["docker run ghcr.io/goplasmatic/orion:latest"]
    end

    style Standalone fill:#21252b,stroke:#5c6370,color:#abb2bf
    style Sidecar fill:#21252b,stroke:#5c6370,color:#abb2bf
    style Container fill:#21252b,stroke:#5c6370,color:#abb2bf
    style S fill:#61afef,stroke:#61afef,color:#1e222b
    style App fill:#4b5263,stroke:#5c6370,color:#abb2bf
    style O fill:#61afef,stroke:#61afef,color:#1e222b
    style D fill:#61afef,stroke:#61afef,color:#1e222b
```

Single binary. SQLite by default, no database to provision, no runtime dependencies. Need more scale? Swap to **PostgreSQL** or **MySQL** by changing the `storage.url`. No rebuild needed.

**Same channel definitions work in any topology:** run everything in one instance, split channels across instances with include/exclude filters, or deploy as sidecars. The definition doesn't change; only the deployment config does.

## Performance

**6K–7K workflow requests/sec** on a single instance (Apple M-series, release build, 50 concurrent connections, v0.2.0 release):

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="media/benchmark-dark.svg">
  <img src="media/benchmark-light.svg" alt="Throughput by scenario — simple workflow: 7,446 req/s; 12 workflows on one channel: 6,912 req/s; complex workflow (4 tasks): 6,053 req/s" width="100%">
</picture>

| Scenario | Req/sec | Avg Latency | P99 Latency |
|----------|--------:|------------:|------------:|
| Simple workflow (1 task) | 7,446 | 6.7 ms | 16.7 ms |
| Complex workflow (4 tasks) | 6,053 | 8.2 ms | 25.5 ms |
| 12 workflows on one channel | 6,912 | 7.2 ms | 16.6 ms |

v0.2.0 upgrades dataflow-rs to 3.0 and datalogic-rs to 5, which moved JSONLogic compilation to engine-construction time. Compared to the v0.1.x baseline (dataflow-rs 2.1.5), complex and multi-workflow scenarios pick up large gains (+48% and +120% req/s respectively) and P99 latency drops materially on every scenario. Run `./tests/benchmark/bench.sh` to reproduce.

Pre-compiled JSONLogic, zero-downtime hot-reload, lock-free reads, SQLite WAL mode, async-first on Tokio.

---

## Use Cases

- **Replace microservices:** define REST endpoints as channels, logic as workflows, external calls as connectors
- **Webhook gateway:** normalize Stripe, GitHub, Shopify payloads into a consistent internal schema
- **Event processing:** Kafka-to-workflow pipelines with transforms, enrichment, and routing
- **API composition:** use `channel_call` to compose services from other services
- **AI-managed business logic:** LLMs create and update workflows via the REST API
- **Multi-agent orchestration:** route agent outputs to channels with coordinating workflows
- **Protocol bridging:** REST-to-Kafka, Kafka-to-HTTP with transformation

See [Use Cases & Patterns](https://goplasmatic.github.io/Orion/tutorials/use-cases.html) for complete, tested examples, or grab ready-to-deploy JSON from [`examples/`](examples/) and run `./deploy.sh <example>` against a local instance.

## Install

```bash
# Docker (quickest way to try)
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest

# Docker Compose (with persistent storage)
docker compose up  # uses docker-compose.yml from this repo

# macOS (Homebrew)
brew install GoPlasmatic/tap/orion-server

# macOS / Linux (shell installer)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.ps1 | iex"

# From crates.io
cargo install orion-server

# From source
cargo install --git https://github.com/GoPlasmatic/Orion.git
```

Verify with `orion-server --version`. Swagger UI available at `http://localhost:8080/docs`. See [Configuration](https://goplasmatic.github.io/Orion/configuration/reference.html) for deployment options.

The server binary also ships diagnostic subcommands you can run without booting the HTTP listener:

```bash
orion-server validate-config -c config.toml         # Parse + validate the config file
orion-server migrate                                # Run pending DB migrations
orion-server migrate --dry-run                      # Preview pending migrations
orion-server lint path/to/workflow.json             # Strict-validate a workflow JSON file
orion-server dry-run -w workflow.json -i input.json # Execute a workflow against a sample payload
orion-server test-connectivity                      # Probe DB (and Kafka if enabled)
orion-server dump-openapi > docs/openapi.json       # Write the OpenAPI 3.1 spec (checked in for offline use / client gen)
```

`${VAR}` / `${VAR:-default}` placeholders inside `config.toml` are substituted from the environment when any of these subcommands load the config, so the same file works across dev, staging, and prod without templating.

### CLI Tool

Manage workflows, channels, and connectors without writing curl commands:

```bash
# Install
brew install GoPlasmatic/tap/orion-cli                # Homebrew
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion-cli/releases/latest/download/orion-cli-installer.sh | sh  # Shell installer
cargo install --git https://github.com/GoPlasmatic/Orion-cli.git  # From source

# Deploy a workflow from a JSON file
orion-cli workflows create -f order-processing.json
orion-cli --yes workflows activate high-value-order
orion-cli channels create -f orders-channel.json
orion-cli --yes channels activate orders
```

See [CLI Reference](https://github.com/GoPlasmatic/Orion-cli) for the full command list.

## Documentation

| Guide | Description |
|-------|-------------|
| [Workflow Reference](https://goplasmatic.github.io/Orion/reference/workflows.html) | Workflow & task JSON schema, conditions, error handling, lifecycle, and rollout |
| [Function Reference](https://goplasmatic.github.io/Orion/reference/functions.html) | Every built-in task function and its exact `input` schema |
| [Portable Data Dialect](https://goplasmatic.github.io/Orion/reference/data-dialect.html) | Backend-neutral query/write envelope for `data_query`/`data_write` — one filter dialect across SQL, MongoDB, Elasticsearch |
| [Admin API](https://goplasmatic.github.io/Orion/api/admin.html) | Workflows, channels, connectors, engine, audit, and backup endpoints |
| [Data API](https://goplasmatic.github.io/Orion/api/data.html) | Data routing, sync/async processing, traces, and operational endpoints |
| [Configuration](https://goplasmatic.github.io/Orion/configuration/reference.html) | Config file, env vars, database backends, deployment |
| [Connectors & Extensibility](https://goplasmatic.github.io/Orion/features/extensibility.html) | HTTP, DB, Cache, Storage, MongoDB, Elasticsearch, Kafka: auth, retry, circuit breakers |
| [Observability](https://goplasmatic.github.io/Orion/features/observability.html) | Prometheus metrics, health checks, Kubernetes probes, tracing, logging |
| [Resilience](https://goplasmatic.github.io/Orion/features/resilience.html) | Circuit breakers, timeouts, dead letter queues |
| [Scalability](https://goplasmatic.github.io/Orion/features/scalability.html) | Rate limiting, backpressure, horizontal scaling |
| [Security](https://goplasmatic.github.io/Orion/features/security.html) | Input validation, SSRF protection, CORS, auth |
| [Deployability](https://goplasmatic.github.io/Orion/features/deployability.html) | Packaging, Docker, installers, distribution |
| [Use Cases & Patterns](https://goplasmatic.github.io/Orion/tutorials/use-cases.html) | AI prompt templates, tested examples, validation workflows, CI/CD |
| [CLI Tool](https://github.com/GoPlasmatic/Orion-cli) | Command-line tool for managing channels, workflows, and connectors |

## Built With

[Axum](https://github.com/tokio-rs/axum) (HTTP), [Tokio](https://tokio.rs) (async runtime), [SQLx](https://github.com/launchbadge/sqlx) (database), [sea-query](https://github.com/SeaQL/sea-query) (portable SQL builder), SQLite/PostgreSQL/MySQL (storage), [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) (JSONLogic), [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) (workflow orchestration).

## Roadmap

The roadmap lives in the issue tracker — every item is an open issue you can 👍 to prioritize, comment on, or pick up:

- **[Web UI](https://github.com/GoPlasmatic/Orion/issues/233):** visual workflow builder and channel dashboard
- **[SDK clients](https://github.com/GoPlasmatic/Orion/issues/234):** Python, Go, and Node.js libraries for the admin API
- **[Workflow marketplace](https://github.com/GoPlasmatic/Orion/issues/235):** community-contributed workflow templates
- **[Scheduler](https://github.com/GoPlasmatic/Orion/issues/236):** cron-based workflow execution
- **[WASM task functions](https://github.com/GoPlasmatic/Orion/issues/237):** extend the engine with custom logic in any language

Have an idea? [Open an issue](https://github.com/GoPlasmatic/Orion/issues) or start a [discussion](https://github.com/GoPlasmatic/Orion/discussions).

## Who's Using Orion?

Using Orion in a project, a company, or a side quest? Add yourself to [ADOPTERS.md](ADOPTERS.md) with a one-line PR, or share what you built in [Discussions](https://github.com/GoPlasmatic/Orion/discussions) — real-world usage reports directly shape the roadmap.

## Contributing

Contributions welcome! Whether it's a bug fix, new connector, documentation improvement, or feature request, we'd love to hear from you.

```bash
cargo build                              # Build (all features included)
cargo build --release                    # Release build
cargo test                               # Run tests
cargo clippy                             # Lint
cargo fmt                                # Format
```

- **Report bugs:** [Open an issue](https://github.com/GoPlasmatic/Orion/issues)
- **Ask questions:** [GitHub Discussions](https://github.com/GoPlasmatic/Orion/discussions)
- **Report security issues privately:** see [SECURITY.md](SECURITY.md)
- **Submit code:** Fork, branch, PR. All tests must pass (`cargo test && cargo clippy`)
- **Docs recordings:** the README GIFs and mdBook asciinema casts are generated from real sessions — see [`docs/recordings/`](docs/recordings/) to regenerate them.

## License

Apache-2.0. See [LICENSE](LICENSE) for details.
