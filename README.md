<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **The declarative services runtime for the AI era.**

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion)](https://github.com/GoPlasmatic/Orion/releases)
  [![GitHub Stars](https://img.shields.io/github/stars/GoPlasmatic/Orion?style=social)](https://github.com/GoPlasmatic/Orion)
</div>

The declarative services runtime — AI generates workflows, Orion provides the governance. Instead of writing, deploying, and operating a microservice for every piece of business logic, you **declare** what the service should do — and Orion runs it. Architectural governance — observability, rate limiting, circuit breakers, versioning, input validation, and more — is built in.

---

## Is Orion Right for You?

| If you need to... | Orion? | Why |
|---|:-:|---|
| Turn business rules into live REST/Kafka services | **Yes** | Define logic as JSON workflows, deploy with one API call |
| Let AI generate and manage business logic | **Yes** | Built-in validation, dry-run testing, and draft-before-activate safety |
| Replace a handful of single-purpose microservices | **Yes** | One instance handles many channels — governance included |
| Use a rule engine like Drools | **Not quite** | Orion uses [JSONLogic](https://jsonlogic.com) via [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) for conditions and transforms — lightweight and AI-friendly, but not a full RETE-based rule engine with complex fact networks |
| Embed a workflow engine library in your app | **No** | Orion is a standalone runtime, not a library. For an embeddable workflow engine, see [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) which Orion is built on |
| Build a visual drag-and-drop workflow UI | No | Orion is API-first; pair it with your own UI or use the [CLI](https://github.com/GoPlasmatic/Orion-cli) |
| Orchestrate long-running jobs (hours/days) | No | Use Temporal or Airflow — Orion is optimized for request-response and event processing |
| Run a full API gateway with plugin ecosystem | No | Use Kong or Envoy — Orion focuses on service logic, not proxy features |
| General-purpose compute (image processing, ML) | No | Orion's task functions operate on JSON data — use custom services or serverless for arbitrary compute |
| Stateful workflows with human-in-the-loop approvals | No | Use [Temporal](https://temporal.io) or BPMN engines — Orion workflows are stateless request pipelines |

---

## Your First Service in 2 Minutes

No code. No Dockerfile. No CI pipeline. Just a running service.

**1. Start Orion**

```bash
brew install GoPlasmatic/tap/orion-server   # or: curl installer, cargo install (see Install)
orion-server
```

**2. Tell AI what you need**

Give your LLM this prompt — or write the JSON yourself:

> "Flag orders over $10,000 for manual review with an alert message"

**3. Deploy the workflow AI generated**

```bash
# Create the workflow (paste the AI-generated JSON here)
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

# Activate it
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**4. Create a channel (the service endpoint)**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "orders", "name": "orders", "channel_type": "sync",
        "protocol": "http", "route_pattern": "/orders",
        "methods": ["POST"], "workflow_id": "high-value-order" }'

# Activate
curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/orders/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**5. Send a request — your service is live**

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

That's it. You described what you needed. AI wrote the logic. Orion ran it — with rate limiting, metrics, health checks, and request tracing already active. Change the threshold? One API call. No rebuild, no redeploy, no restart.

---

## Three Primitives

You build services in Orion with three things:

```
┌─────────────┐       ┌──────────────┐       ┌─────────────┐
│   Channel   │──────▶│  Workflow    │──────▶│  Connector  │
│  (endpoint) │       │  (logic)     │       │  (external) │
└─────────────┘       └──────────────┘       └─────────────┘
```

| Primitive | What it is | Example |
|-----------|-----------|---------|
| **Channel** | A service endpoint — sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | A pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | A named connection to an external system, with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions — all through the admin API.

**Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability — automatically.

---

## AI Writes Services, Not Code

When AI generates a microservice, you still need to add health checks, metrics, retries, and error handling. When AI generates an Orion workflow, **all of that is already there**. The platform guarantees it.

**Use the [Orion CLI's MCP server](https://github.com/GoPlasmatic/Orion-cli)** to give your AI assistant full Orion context — no manual prompt engineering needed. The MCP server provides your LLM with workflow syntax, available functions, connector types, and API operations automatically.

```
You: "Classify orders into VIP (>=500, 15% discount), Premium (100-500, 5%), and Standard tiers"

AI:  → generates valid workflow JSON
     → creates it via the API
     → tests with dry-run
     → activates when you approve
```

**Safe path from AI output to production — every time:**

```
1. Generate    → AI produces the workflow JSON (with full context from MCP)
2. Validate    → POST /api/v1/admin/workflows/validate     ← catches structural errors
3. Create      → POST /api/v1/admin/workflows               ← saved as draft (not live)
4. Dry-run     → POST /api/v1/admin/workflows/{id}/test     ← test with sample data
5. Activate    → PATCH /api/v1/admin/workflows/{id}/status   ← goes live
6. Rollout     → PATCH /api/v1/admin/workflows/{id}/rollout  ← gradual traffic (10% → 50% → 100%)
```

Every AI-generated workflow gets version history, draft-before-activate, dry-run testing, rollout control, and audit trails — the same governance as hand-written ones. Roll back to any previous version instantly.

See [Use Cases & Patterns](docs/src/tutorials/use-cases.md#ai-workflow--cicd) for CI/CD integration and GitHub Actions examples.

---

## Before & After

**Before** — every piece of business logic is its own service to build, deploy, and operate:

```
                         ┌──────────────┐
                    ┌───▶│ Pricing Svc  │───▶ DB
                    │    └──────────────┘
┌──────────┐        │    ┌──────────────┐
│  API     │────────┼───▶│ Fraud Svc    │───▶ Redis
│  Gateway │        │    └──────────────┘
└──────────┘        │    ┌──────────────┐
                    ├───▶│ Routing Svc  │───▶ Kafka
                    │    └──────────────┘
                    │    ┌──────────────┐
                    └───▶│ Notif. Svc   │───▶ SMTP
                         └──────────────┘

4 services × (code + Dockerfile + CI + health checks + metrics + deployment)
```

**After** — one Orion instance replaces all four:

```
                         ┌──────────────────────────────────┐
                         │           Orion                  │
                         │                                  │
┌──────────┐             │  Channel /pricing  → workflow ───┼──▶ DB
│ Clients  │────────────▶│  Channel /fraud    → workflow ───┼──▶ Redis
└──────────┘             │  Channel /routing  → workflow ───┼──▶ Kafka
                         │  Channel /notify   → workflow ───┼──▶ SMTP
                         │                                  │
                         │  Rate limiting, metrics, health  │
                         │  checks, circuit breakers, logs  │
                         └──────────────────────────────────┘

No API gateway needed. Governance is built in. One binary to deploy.
```

**The best of both worlds:** each channel and workflow is independently versioned, testable, and deployable — the modularity of microservices with the operational simplicity of a monolith. Change one workflow without touching the others. Roll back a single channel without redeploying everything.

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
| **Observability** | Prometheus metrics, structured logs, distributed tracing | Always on — zero configuration |
| **Health checks** | Component-level status with degradation detection | `GET /health` — automatic |
| **Request IDs** | UUID propagated through the entire pipeline | `x-request-id` header — automatic |
| **Deduplication** | Prevent duplicate processing via idempotency keys | `Idempotency-Key` header, configurable retention window |
| **Response caching** | Cache responses for identical requests | TTL-based, configurable cache key fields |

A minimal channel needs only a name and a workflow. Everything else has sensible defaults.

> **Observability deep dive:** health endpoints, full Prometheus metrics list, Kubernetes probes, and OpenTelemetry tracing config — see [Observability Guide](docs/src/features/observability.md).

---

## Sync and Async

```
Sync     POST /api/v1/data/{channel}         → immediate response
Async    POST /api/v1/data/{channel}/async   → returns trace_id, poll later

REST     GET /api/v1/data/orders/{id}        → matched by route pattern
Kafka    topic: order.placed                 → consumed automatically
```

Sync channels respond immediately. Async channels return a trace ID — poll `GET /api/v1/data/traces/{id}` for results. Kafka channels consume from topics configured in the DB or config file — no restart needed when you add new ones.

**Bridging is a pattern, not a feature.** A sync workflow can `publish_kafka` and return 202. An async channel picks it up from there.

REST channels support parameterized route patterns (`/orders/{order_id}`) with path, query, and header injection into the workflow context — see [Data API](docs/src/api/data.md#route-resolution).

## Service Composition

Most platforms require HTTP calls between services — adding latency, failure modes, and serialization overhead. Orion's `channel_call` invokes another channel's workflow **in-process** with zero network round-trip:

```
POST /orders (order-processing workflow)
  ├── parse_json         → extract order data
  ├── channel_call       → "inventory-check" channel (in-process)
  ├── channel_call       → "customer-lookup" channel (in-process)
  ├── map                → compute pricing with enriched data
  └── publish_json       → return combined result
```

Each composed channel has its own workflow, versioning, and governance — but calls between them are function calls, not network hops. Cycle detection prevents infinite recursion.

---

## Connect to Anything

Connectors are named, reusable connections to external systems. Configure once, reference by name in any workflow — credentials stay out of your logic:

| Connector type | Systems | Features |
|---------------|---------|----------|
| **HTTP** | Any REST API, webhook, or service | Bearer / Basic / API key auth, retry with backoff, SSRF protection |
| **Database** | PostgreSQL, MySQL, SQLite | Parameterized queries, connection pooling, read + write operations |
| **Cache** | In-memory (built-in) or Redis | TTL-based expiry, also powers deduplication and response caching |
| **MongoDB** | Any MongoDB instance | Document queries, BSON-to-JSON conversion, connection pooling |
| **Kafka** | Any Kafka cluster | Publish with key/value logic, consume with DLQ routing |

Every connector gets **circuit breaker protection** automatically — failures trip the breaker, subsequent calls fast-fail, and the breaker auto-recovers. Secrets are stored in the database and masked in API responses. See [Connectors Guide](docs/src/features/extensibility.md#connectors) for configuration examples and auth options.

---

## Built-in Task Functions

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context for downstream tasks |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields, constraints, and schema-like checks |
| `http_call` | Invoke downstream APIs, webhooks, or services via [connectors](docs/src/features/extensibility.md#connectors) |
| `channel_call` | Invoke another channel's workflow in-process |
| `db_read` | Execute SQL SELECT queries, return rows as JSON |
| `db_write` | Execute SQL INSERT/UPDATE/DELETE, return affected count |
| `cache_read` | Read from in-memory or Redis cache |
| `cache_write` | Write to cache with optional TTL |
| `mongo_read` | Query MongoDB collections, BSON-to-JSON conversion |
| `publish_json` | Serialize data to JSON output format |
| `publish_xml` | Serialize data to XML output format |
| `publish_kafka` | Publish messages to [Kafka topics](docs/src/features/extensibility.md#kafka-connector) |
| `log` | Emit structured log entries for auditing and debugging |

All functions are built in. `cache_read`/`cache_write` use in-memory cache by default; configure a Redis-backed connector for distributed caching.

---

## When Things Go Wrong

Production services fail. Orion handles it so you don't write retry loops and fallback logic:

| Failure | What Orion does | You configure |
|---------|----------------|---------------|
| **External API down** | Circuit breaker trips, fast-fails subsequent calls, auto-recovers | `failure_threshold`, `recovery_timeout_secs` per connector |
| **Slow workflow** | Timeout fires, returns 504 to caller | `timeout_ms` per channel |
| **Traffic spike** | Rate limiter rejects excess requests (429), backpressure sheds load (503) | `requests_per_second`, `max_concurrent` per channel |
| **Async task fails** | Moved to Dead Letter Queue, retried automatically with backoff | `dlq_max_retries`, `dlq_poll_interval_secs` |
| **Task in pipeline fails** | Pipeline halts with error — or continues collecting errors if `continue_on_error: true` | Per-workflow setting |
| **Duplicate request** | Detected via idempotency key, returns 409 | `Idempotency-Key` header + retention window |

**Debugging is built in.** Every request gets a `x-request-id` propagated through the entire pipeline. Structured JSON logs show what data each task received and produced. Enable OpenTelemetry for distributed tracing across `http_call` and `channel_call` chains. Inspect circuit breakers, DLQ traces, and debug endpoints via the [API Reference](docs/src/api/admin.md).

---

## Deploy Anywhere

```
┌────────────────┐   ┌────────────────────┐   ┌────────────────┐
│   Standalone   │   │      Sidecar       │   │     Docker     │
│                │   │                    │   │                │
│ ./orion-server │   │  ┌─────┐ ┌──────┐  │   │  docker run \  │
│                │   │  │ App │─│Orion │  │   │   orion:latest │
│   That's it.   │   │  └─────┘ └──────┘  │   │                │
└────────────────┘   └────────────────────┘   └────────────────┘
```

Single binary. SQLite by default — no database to provision, no runtime dependencies. Need more scale? Swap to **PostgreSQL** or **MySQL** by changing the `storage.url` — no rebuild needed.

**Same channel definitions work in any topology** — run everything in one instance, split channels across instances with include/exclude filters, or deploy as sidecars. The definition doesn't change; only the deployment config does.

## Performance

**100K+ requests/sec** on a single instance (Apple M2 Pro, release build, 50 concurrent connections):

| Scenario | Req/sec | Avg Latency | P99 Latency |
|----------|--------:|------------:|------------:|
| Simple workflow (1 task) | 100,882 | 1.50 ms | 1.30 ms |
| Complex workflow (5 tasks) | 85,768 | 1.50 ms | 1.60 ms |
| 12 workflows on one channel | 90,023 | 1.50 ms | 1.60 ms |

Pre-compiled JSONLogic, zero-downtime hot-reload, lock-free reads, SQLite WAL mode, async-first on Tokio. Run `./tests/benchmark/bench.sh` to reproduce.

---

## Use Cases

- **Replace microservices** — define REST endpoints as channels, logic as workflows, external calls as connectors
- **Webhook gateway** — normalize Stripe, GitHub, Shopify payloads into a consistent internal schema
- **Event processing** — Kafka-to-workflow pipelines with transforms, enrichment, and routing
- **API composition** — use `channel_call` to compose services from other services
- **AI-managed business logic** — LLMs create and update workflows via the REST API
- **Multi-agent orchestration** — route agent outputs to channels with coordinating workflows
- **Protocol bridging** — REST-to-Kafka, Kafka-to-HTTP with transformation

See [Use Cases & Patterns](docs/src/tutorials/use-cases.md) for complete, tested examples.

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

Verify with `orion-server --version`. Swagger UI available at `http://localhost:8080/docs`. See [Configuration](docs/src/configuration/reference.md) for deployment options.

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
| [Admin API](docs/src/api/admin.md) | Workflows, channels, connectors, engine, audit, and backup endpoints |
| [Data API](docs/src/api/data.md) | Data routing, sync/async processing, traces, and operational endpoints |
| [Configuration](docs/src/configuration/reference.md) | Config file, env vars, database backends, deployment |
| [Connectors & Extensibility](docs/src/features/extensibility.md) | HTTP, DB, Cache, Storage, MongoDB, Kafka — auth, retry, circuit breakers |
| [Observability](docs/src/features/observability.md) | Prometheus metrics, health checks, Kubernetes probes, tracing, logging |
| [Resilience](docs/src/features/resilience.md) | Circuit breakers, timeouts, dead letter queues |
| [Scalability](docs/src/features/scalability.md) | Rate limiting, backpressure, horizontal scaling |
| [Security](docs/src/features/security.md) | Input validation, SSRF protection, CORS, auth |
| [Deployability](docs/src/features/deployability.md) | Packaging, Docker, installers, distribution |
| [Use Cases & Patterns](docs/src/tutorials/use-cases.md) | AI prompt templates, tested examples, validation workflows, CI/CD |
| [CLI Tool](https://github.com/GoPlasmatic/Orion-cli) | Command-line tool for managing channels, workflows, and connectors |

## Built With

[Axum](https://github.com/tokio-rs/axum) (HTTP), [Tokio](https://tokio.rs) (async runtime), [SQLx](https://github.com/launchbadge/sqlx) (database), [sea-query](https://github.com/SeaQL/sea-query) (portable SQL builder), SQLite/PostgreSQL/MySQL (storage), [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) (JSONLogic), [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) (workflow orchestration).

## Roadmap

- **Web UI** — visual workflow builder and channel dashboard
- **SDK clients** — Python, Go, and Node.js libraries for the admin API
- **Workflow marketplace** — community-contributed workflow templates
- **Scheduler** — cron-based workflow execution
- **WASM task functions** — extend the engine with custom logic in any language

Have an idea? [Open a discussion](https://github.com/GoPlasmatic/Orion/issues).

## Contributing

Contributions welcome! Whether it's a bug fix, new connector, documentation improvement, or feature request — we'd love to hear from you.

```bash
cargo build                              # Build (all features included)
cargo build --release                    # Release build
cargo test                               # Run tests
cargo clippy                             # Lint
cargo fmt                                # Format
```

- **Report bugs:** [Open an issue](https://github.com/GoPlasmatic/Orion/issues)
- **Ask questions:** [Start a discussion](https://github.com/GoPlasmatic/Orion/issues)
- **Submit code:** Fork, branch, PR — all tests must pass (`cargo test && cargo clippy`)

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
