<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>The declarative services runtime for the AI era</p>
</div>

**Orion** is a declarative services runtime written in Rust. Instead of writing, deploying, and operating a microservice for every piece of business logic, you **declare** what the service should do, and Orion runs it. Architectural governance — observability, rate limiting, circuit breakers, versioning, input validation, and more — is built in.

AI generates workflows, Orion provides the governance. The platform guarantees that every service gets health checks, metrics, retries, and error handling, regardless of how the workflow was created.

## Is Orion Right for You?

| If you need to... | Orion? | Why |
|---|:-:|---|
| Turn business rules into live REST/Kafka services | **Yes** | Define logic as JSON workflows, deploy with one API call |
| Let AI generate and manage business logic | **Yes** | Built-in validation, dry-run testing, and draft-before-activate safety |
| Replace a handful of single-purpose microservices | **Yes** | One instance handles many channels, governance included |
| Use a rule engine like Drools | Not quite | Orion uses [JSONLogic](https://jsonlogic.com). Lightweight and AI-friendly, but not a full RETE-based rule engine |
| Embed a workflow engine library in your app | No | Orion is a standalone runtime. For an embeddable engine, see [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) |
| Orchestrate long-running jobs (hours/days) | No | Use Temporal or Airflow. Orion is optimized for request-response and event processing |
| Run a full API gateway with plugin ecosystem | No | Use Kong or Envoy. Orion focuses on service logic, not proxy features |

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

**Design-time:** define channels, build workflows, configure connectors, test with dry-run, manage versions, all through the admin API.

**Runtime:** Orion routes traffic to channels, executes workflows, calls connectors, and handles observability automatically.

## Your First Service in 2 Minutes

No code. No Dockerfile. No CI pipeline.

**1. Start Orion**

```bash
brew install GoPlasmatic/tap/orion-server   # or: curl installer, cargo install
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
| **Observability** | Prometheus metrics, structured logs, distributed tracing | Always on, zero configuration |
| **Health checks** | Component-level status with degradation detection | `GET /health`, automatic |
| **Deduplication** | Prevent duplicate processing via idempotency keys | `Idempotency-Key` header, configurable window |
| **Response caching** | Cache responses for identical requests | TTL-based, configurable cache key fields |
| **Per-request profiling** | Break a single request down by phase (lock, run, tasks) | `X-Orion-Profile: 1` header, response under `_orion.profile` |
| **Per-task tracing** | Record each task's input/output for replay and debugging | Channel `config.tracing.per_task`, stored on the trace row |

## Performance

**6K–7K workflow requests/sec** on a single instance (Apple M-series, release build, 50 concurrent connections, v0.2.0 release):

| Scenario | Req/sec | Avg Latency | P99 Latency |
|----------|--------:|------------:|------------:|
| Simple workflow (1 task) | 7,446 | 6.7 ms | 16.7 ms |
| Complex workflow (4 tasks) | 6,053 | 8.2 ms | 25.5 ms |
| 12 workflows on one channel | 6,912 | 7.2 ms | 16.6 ms |

v0.2.0 ships on dataflow-rs 3.0 / datalogic-rs 5, which moves JSONLogic compilation to engine-construction time. Compared to the v0.1.x baseline, complex and multi-workflow scenarios pick up roughly +48% and +120% req/s respectively, and P99 latency improves on every scenario. Per-version benchmark folders live under `tests/benchmark/results/`.

Pre-compiled JSONLogic, zero-downtime hot-reload, lock-free reads, SQLite WAL mode, async-first on Tokio.
