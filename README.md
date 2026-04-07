<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **The declarative services runtime for the AI era.**

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion)](https://github.com/GoPlasmatic/Orion/releases)
</div>

The declarative services runtime — AI generates workflows, Orion provides the governance. Instead of writing, deploying, and operating a microservice for every piece of business logic, you **declare** what the service should do — and Orion runs it. Architectural governance — observability, rate limiting, circuit breakers, versioning, input validation, and more — is built in.

---

## Quick Start

```bash
brew install GoPlasmatic/tap/orion   # or: curl installer, cargo install (see Install)
orion-server
```

**Tell AI what you need:**

```
"Flag orders over $10,000 for manual review with an alert message"
```

**AI generates the workflow — deploy it with two API calls:**

```bash
# 1. Create the workflow
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
    "id": "high-value-order",
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

# 2. Activate it
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**Create a channel (the service endpoint) and activate it:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "name": "orders", "channel_type": "sync", "protocol": "http",
        "methods": ["POST"], "workflow_id": "high-value-order" }'

# Activate (grab the channel_id from the response above)
curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/<channel-id>/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**Push data — your service is live:**

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

You described what you needed. AI wrote the logic. You pushed JSON — Orion handled the rest. Change the threshold? One API call. No rebuild, no redeploy, no restart.

---

## Three Primitives

You build services in Orion with three things:

```
┌─────────────┐       ┌─────────────┐       ┌─────────────┐
│   Channel   │──────▶│  Workflow    │──────▶│  Connector  │
│  (endpoint) │       │  (logic)    │       │  (external) │
└─────────────┘       └─────────────┘       └─────────────┘
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

**Give AI this system prompt:**

```
You generate Orion workflows in JSON. Workflows have:
- name, condition (JSONLogic or true)
- tasks: array of { id, name, condition (optional), function: { name, input } }
- Start with parse_json: { "name": "parse_json", "input": { "source": "payload", "target": "<entity>" } }
- Use "map" for transforms, "http_call" with "connector" for APIs, "channel_call" for service composition.
- Task conditions: { "var": "data.<entity>.<field>" }
Output ONLY the JSON.
```

**Then validate before going live:**

```
1. Generate    → LLM produces the workflow JSON
2. Validate    → POST /api/v1/admin/workflows/validate
3. Create      → POST /api/v1/admin/workflows (created as draft)
4. Dry-run     → POST /api/v1/admin/workflows/{id}/test
5. Activate    → PATCH /api/v1/admin/workflows/{id}/status
```

Every AI-generated workflow gets version history, draft-before-activate, dry-run testing, and audit trails — the same governance as hand-written ones. See [Use Cases & Patterns](docs/use-cases.md#ai-workflow--cicd) for prompt templates, CI/CD integration, and GitHub Actions examples.

---

## Before & After

**Before** — every piece of business logic is a service to build, deploy, and operate:

```
pricing-service/      → 800 lines + Dockerfile + CI + health checks + metrics
fraud-service/        → 600 lines + Dockerfile + CI + health checks + metrics
routing-service/      → 400 lines + Dockerfile + CI + health checks + metrics
notification-service/ → 500 lines + Dockerfile + CI + health checks + metrics

4 services × (governance + deployment + monitoring) = operational burden
```

**After** — declare what each service does, Orion runs it:

```
One Orion instance:
  Channel "POST /orders"       → order processing workflow
  Channel "POST /payments"     → payment validation workflow
  Channel "GET /users/{id}"    → user lookup workflow
  Channel topic:order.placed   → async fulfillment workflow

Governance? Built in. Metrics? Automatic. Deploy? Already running.
```

---

## What You Get for Free

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

A minimal channel needs only a name and a workflow. Everything else has sensible defaults.

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

## REST Route Matching

Channels can define full REST route patterns with parameter extraction:

```json
{
  "name": "order-detail",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["GET"],
  "route_pattern": "/orders/{order_id}",
  "workflow_id": "order-lookup"
}
```

Path parameters (`order_id`), query parameters, and headers are injected into the workflow context — available to conditions, transforms, and validation logic.

## Service Composition

Use `channel_call` to invoke another channel's workflow in-process — no HTTP round-trip:

```json
{
  "id": "enrich",
  "function": {
    "name": "channel_call",
    "input": {
      "channel": "customer-lookup",
      "data_logic": { "var": "data.order.customer_id" },
      "response_path": "data.customer"
    }
  }
}
```

Compose services from other services. Same interface, same governance, zero network overhead.

---

## Built-in Task Functions

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context for downstream tasks |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields, constraints, and schema-like checks |
| `http_call` | Invoke downstream APIs, webhooks, or services via [connectors](docs/connectors.md) |
| `channel_call` | Invoke another channel's workflow in-process |
| `db_read` | Execute SQL SELECT queries, return rows as JSON `*` |
| `db_write` | Execute SQL INSERT/UPDATE/DELETE, return affected count `*` |
| `cache_read` | Read from Redis cache `*` |
| `cache_write` | Write to Redis cache with optional TTL `*` |
| `mongo_read` | Query MongoDB collections, BSON-to-JSON conversion `*` |
| `publish_json` | Serialize data to JSON output format |
| `publish_xml` | Serialize data to XML output format |
| `publish_kafka` | Publish messages to [Kafka topics](docs/kafka.md) |
| `log` | Emit structured log entries for auditing and debugging |

`*` Requires feature flag: `connectors-sql`, `connectors-redis`, or `connectors-mongodb`. Handler code is implemented; engine registration is pending.

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

Single binary. SQLite by default — no database to provision, no runtime dependencies. Need more scale? Swap to **PostgreSQL** or **MySQL** with a feature flag at build time.

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

See [Use Cases & Patterns](docs/use-cases.md) for complete, tested examples.

## Install

```bash
# macOS (Homebrew)
brew install GoPlasmatic/tap/orion

# macOS / Linux (shell installer)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-installer.ps1 | iex"

# From source (SQLite default)
cargo install --git https://github.com/GoPlasmatic/Orion.git

# From source (PostgreSQL backend)
cargo install --git https://github.com/GoPlasmatic/Orion.git --no-default-features --features db-postgres,swagger-ui
```

Verify with `orion-server --version`. See [Configuration](docs/configuration.md#deployment) for Docker and deployment options.

## Documentation

| Guide | Description |
|-------|-------------|
| [API Reference](docs/api-reference.md) | Channels, workflows, connectors, data, and operational endpoints |
| [Configuration](docs/configuration.md) | Config file, env vars, database backends, deployment, and production checklist |
| [Connectors](docs/connectors.md) | HTTP, DB, Cache, Storage, Kafka — auth, retry, and secret masking |
| [Kafka Integration](docs/kafka.md) | Topic mapping, DB-driven consumers, DLQ, and publishing |
| [Production Features](docs/production-features.md) | Versioning, rollout, channel config, REST routing, fault tolerance |
| [Use Cases & Patterns](docs/use-cases.md) | AI prompt templates, tested examples, validation workflows, CI/CD |
| [Observability](docs/observability.md) | Prometheus metrics, health checks, engine status, and logging |
| [CLI Tool](https://github.com/GoPlasmatic/Orion-cli) | Command-line tool for managing channels, workflows, and connectors |

## Built With

[Axum](https://github.com/tokio-rs/axum) (HTTP), [Tokio](https://tokio.rs) (async runtime), [SQLx](https://github.com/launchbadge/sqlx) (database), [sea-query](https://github.com/SeaQL/sea-query) (portable SQL builder), SQLite/PostgreSQL/MySQL (storage), [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) (JSONLogic), [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) (workflow orchestration).

## Contributing

Contributions welcome. [Open an issue](https://github.com/GoPlasmatic/Orion/issues) or submit a pull request.

```bash
cargo build                              # Build (SQLite default)
cargo build --features kafka             # Build with Kafka support
cargo build --features connectors-sql    # Build with external SQL connectors
cargo test                               # Run tests
cargo clippy                             # Lint
cargo fmt                                # Format
```

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
