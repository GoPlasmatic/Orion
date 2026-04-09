# Introduction

**Orion** is a declarative services runtime written in Rust. Instead of writing, deploying, and operating a microservice for every piece of business logic, you **declare** what the service should do — and Orion runs it. Architectural governance — observability, rate limiting, circuit breakers, versioning, input validation, and more — is built in.

AI generates workflows, Orion provides the governance. The platform guarantees that every service gets health checks, metrics, retries, and error handling — regardless of how the workflow was created.

## Is Orion Right for You?

| If you need to... | Orion? | Why |
|---|:-:|---|
| Turn business rules into live REST/Kafka services | **Yes** | Define logic as JSON workflows, deploy with one API call |
| Let AI generate and manage business logic | **Yes** | Built-in validation, dry-run testing, and draft-before-activate safety |
| Replace a handful of single-purpose microservices | **Yes** | One instance handles many channels — governance included |
| Use a rule engine like Drools | Not quite | Orion uses [JSONLogic](https://jsonlogic.com) — lightweight and AI-friendly, but not a full RETE-based rule engine |
| Embed a workflow engine library in your app | No | Orion is a standalone runtime. For an embeddable engine, see [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) |
| Orchestrate long-running jobs (hours/days) | No | Use Temporal or Airflow — Orion is optimized for request-response and event processing |
| Run a full API gateway with plugin ecosystem | No | Use Kong or Envoy — Orion focuses on service logic, not proxy features |

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
| **Channel** | A service endpoint — sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | A pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | A named connection to an external system, with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions — all through the admin API.

**Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability — automatically.

## Your First Service in 2 Minutes

No code. No Dockerfile. No CI pipeline.

**1. Start Orion**

```bash
brew install GoPlasmatic/tap/orion   # or: curl installer, cargo install
orion-server
```

**2. Create a workflow** (AI-generated or hand-written)

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
            { "path": "data.order.alert", "logic": {
              "cat": ["High-value order: $", { "var": "data.order.total" }]
            }}
          ]}
      }}
    ]
  }'

# Activate it
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**3. Create a channel** (the service endpoint)

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

**4. Send a request — your service is live**

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

That's it. Rate limiting, metrics, health checks, and request tracing are already active. Change the threshold? One API call. No rebuild, no redeploy, no restart.

## What's Built In

Every channel gets production-grade features without writing a line of code:

| Feature | What it does | Configuration |
|---------|-------------|---------------|
| **Rate limiting** | Throttle requests per client or globally | `requests_per_second`, `burst`, JSONLogic key |
| **Timeouts** | Cancel slow workflows, return 504 | `timeout_ms` per channel |
| **Input validation** | Reject bad requests at the boundary | JSONLogic with headers, query, path access |
| **Backpressure** | Shed load when overwhelmed, return 503 | `max_concurrent` (semaphore-based) |
| **CORS** | Control browser cross-origin access | `allowed_origins` per channel |
| **Circuit breakers** | Stop cascading failures to external services | Automatic per connector, admin API to inspect/reset |
| **Versioning** | Draft → active → archived lifecycle | Automatic version history, rollout percentages |
| **Observability** | Prometheus metrics, structured logs, distributed tracing | Always on — zero configuration |
| **Health checks** | Component-level status with degradation detection | `GET /health` — automatic |
| **Deduplication** | Prevent duplicate processing via idempotency keys | `Idempotency-Key` header, configurable window |
| **Response caching** | Cache responses for identical requests | TTL-based, configurable cache key fields |

## Performance

**100K+ requests/sec** on a single instance (Apple M2 Pro, release build, 50 concurrent connections):

| Scenario | Req/sec | Avg Latency | P99 Latency |
|----------|--------:|------------:|------------:|
| Simple workflow (1 task) | 100,882 | 1.50 ms | 1.30 ms |
| Complex workflow (5 tasks) | 85,768 | 1.50 ms | 1.60 ms |
| 12 workflows on one channel | 90,023 | 1.50 ms | 1.60 ms |

Pre-compiled JSONLogic, zero-downtime hot-reload, lock-free reads, SQLite WAL mode, async-first on Tokio.
