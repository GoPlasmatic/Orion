<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion Rule Engine

  **A lightweight, AI-ready rules engine. Single binary. Pure REST API. Zero dependencies.**

  Externalize business logic as JSON rules. Deploy as a standalone service or sidecar.
  Manage rules programmatically — from your code, your UI, or your AI agents.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
</div>

---

## Why Orion?

| Feature | |
|---------|---|
| **AI-Ready** | JSON rules that LLMs can generate, validate, and deploy via API |
| **Single Binary** | No runtime dependencies — embedded SQLite with WAL mode |
| **Hot-Reload** | Update rules at runtime — zero downtime |
| **JSONLogic Standard** | Portable, safe, language-agnostic rule conditions |
| **Task Pipelines** | Chain validation, transforms, API calls, and publishing |
| **Prometheus Metrics** | Built-in counters, histograms, and health checks |
| **Connectors** | Named external service configs — secrets stay out of rules |
| **Sync, Async & Batch** | Three processing modes for any workload |

## The Problem

Business logic hardcoded in your application is painful:

- **Scattered if/else chains** — pricing rules, validation, routing decisions buried across services
- **Coupled to deployments** — changing a discount threshold means a full release cycle
- **Hard to audit** — no single place to see what rules exist or what they do
- **Impossible for non-devs to change** — every tweak requires a developer
- **Duplicated across languages** — the same logic reimplemented in every service

## The Solution

Deploy the rule engine as a sidecar or standalone service. Define rules as JSON with JSONLogic conditions. Push data via REST or Kafka. Rules evaluate instantly, trigger actions, and return results. Change rules at runtime — no restarts, no redeployments. Let AI agents create and manage rules through the same API your code uses.

```
Your App (any language)
        │
        │  POST /api/v1/data/orders
        │  { "data": { "total": 25000 } }
        ▼
┌──────────────────────────────────────────┐
│           Orion Rule Engine              │
│                                          │
│   Channel Router → Rule Matcher          │
│        → Task Pipeline → Output          │
└──────────────────────────────────────────┘
        │
        ▼
  Webhooks, APIs, Kafka, HTTP response
```

Your application stays focused on what it does best. The rule engine handles the decisions.

## AI-Ready by Design

Orion is built from the ground up for programmatic rule management — making it a natural fit for AI agents, LLM tool calling, and automated workflows.

### LLMs Can Generate Rules

JSONLogic is a [well-known standard](https://jsonlogic.com) that LLMs already understand. AI agents can generate valid rules from natural language:

```
Prompt: "Flag orders over $10,000 from new customers for manual review"

Generated JSONLogic condition:
{ "and": [
  { ">": [{ "var": "data.total" }, 10000] },
  { "<": [{ "var": "data.account_age_days" }, 30] }
]}
```

### Full Lifecycle via REST API

Create, test, activate, update, and delete rules — all through the API. No file editing, no deployments:

```bash
# 1. Create a rule (starts as active)
curl -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{ "name": "High-risk review", "channel": "orders", ... }'

# 2. Dry-run against sample data
curl -X POST http://localhost:8080/api/v1/admin/rules/{id}/test \
  -H "Content-Type: application/json" \
  -d '{ "data": { "total": 25000, "account_age_days": 5 } }'

# 3. Pause or reactivate as needed
curl -X PATCH http://localhost:8080/api/v1/admin/rules/{id}/status \
  -H "Content-Type: application/json" \
  -d '{ "status": "paused" }'
```

### Dry-Run Testing for Safety

The `POST /api/v1/admin/rules/{id}/test` endpoint lets AI agents validate rules before they go live. Send sample payloads, inspect results, and confirm behavior — without affecting production traffic.

### Structured I/O for Tool Calling

Every API endpoint accepts and returns well-structured JSON. Error responses follow a consistent format. This makes Orion ideal as a function-calling target for AI agents, an MCP tool, or a node in a multi-agent orchestration pipeline.

## Quick Start

### 1. Run the Engine

```bash
# Zero-config — all defaults are sensible
cargo run

# Or with a config file
cargo run -- --config ./config.toml

# Or with Docker
docker build -t orion .
docker run -p 8080:8080 orion
```

### 2. Create a Connector

```bash
curl -X POST http://localhost:8080/api/v1/admin/connectors \
  -H "Content-Type: application/json" \
  -d '{
    "name": "slack_webhook",
    "type": "http",
    "config": {
      "base_url": "https://hooks.slack.com/services",
      "timeout_ms": 3000
    }
  }'
```

### 3. Create a Rule

```bash
curl -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{
    "name": "High-Value Order Alert",
    "channel": "orders",
    "priority": 10,
    "condition": { ">": [{ "var": "data.total" }, 10000] },
    "tasks": [
      {
        "id": "validate",
        "function": "validation",
        "config": {
          "rules": [{
            "path": "data.customer_id",
            "logic": { "!!": { "var": "data.customer_id" } },
            "message": "customer_id is required"
          }]
        }
      },
      {
        "id": "transform",
        "function": "map",
        "config": {
          "mappings": [{
            "path": "data.alert_message",
            "logic": { "cat": ["High-value order: $", { "var": "data.total" }] }
          }]
        }
      },
      {
        "id": "notify",
        "function": "http_call",
        "config": {
          "connector": "slack_webhook",
          "method": "POST",
          "path": "/T00000/B00000/XXXX",
          "body": { "text": "{{ data.alert_message }}" }
        }
      }
    ]
  }'
```

### 4. Push Data

```bash
curl -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{
    "data": {
      "order_id": "ORD-9182",
      "customer_id": "C-1234",
      "total": 25000,
      "currency": "USD"
    }
  }'
```

That's it. The rule fires, validates the payload, builds an alert message, and sends it to Slack. Your application didn't need to know any of that logic — it just pushed data.

## How It Works

```
Inbound (REST or Kafka)
        │
        ▼
   Channel Router
   (route by topic: "orders", "events", "alerts")
        │
        ▼
   Rule Matcher
   (JSONLogic conditions evaluated against your data)
        │
        ▼
   Task Pipeline
   (validate → transform → enrich → call APIs → publish)
        │
        ▼
   Output (HTTP response / Kafka / webhooks)
```

1. Your application sends a JSON message via the REST API or Kafka
2. The message routes to a **channel** (e.g., `orders`, `events`, `alerts`)
3. Rules with matching JSONLogic conditions fire
4. Each matched rule executes an ordered **task pipeline** — validate, transform, enrich, call external APIs, publish
5. Results return synchronously or asynchronously

## JSONLogic

[JSONLogic](https://jsonlogic.com) is an open standard for expressing logic as JSON. It's portable, language-agnostic, and safe to execute.

```json
{ ">": [{ "var": "data.total" }, 10000] }
```

This reads: *"is `data.total` greater than 10,000?"*

JSONLogic supports arithmetic, string operations, array operations, boolean logic, and variable access. Because rules are plain JSON, they can be:

- **Stored** in any database
- **Serialized** over any transport
- **Generated** by UIs, scripts, or AI agents
- **Evaluated** without compiling or interpreting code

Orion pre-compiles JSONLogic conditions for near-zero evaluation overhead at runtime.

## Built-in Task Functions

Chain these functions in your rule's task pipeline:

| Function | Description |
|----------|-------------|
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields, constraints, and schema-like checks |
| `filter` | Allow or deny processing based on conditions |
| `parse_json` | Parse string payloads into structured JSON |
| `parse_xml` | Parse XML payloads into structured JSON |
| `publish_json` | Serialize data to JSON output format |
| `publish_xml` | Serialize data to XML output format |
| `enrich` | Fetch external data via HTTP and merge it into the message |
| `http_call` | Invoke downstream APIs, webhooks, or services |
| `publish_kafka` | Publish messages to Kafka topics |
| `log` | Emit structured log entries for auditing and debugging |

## Performance

Orion is designed for low-latency, high-throughput rule evaluation:

- **Pre-compiled JSONLogic** — rule conditions are compiled once at load time, not interpreted per request
- **Zero-downtime reloads** — double-Arc engine swap lets readers continue on the old engine while the new one loads
- **In-memory connector registry** — O(1) lookup for external service configurations
- **SQLite WAL mode** — concurrent reads and writes without blocking
- **Async-first runtime** — built on Tokio for non-blocking I/O across all operations

## API Reference

### Admin API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/rules` | Create rule |
| GET | `/api/v1/admin/rules` | List and filter rules |
| GET | `/api/v1/admin/rules/{id}` | Get rule by ID |
| PUT | `/api/v1/admin/rules/{id}` | Update rule (creates new version) |
| DELETE | `/api/v1/admin/rules/{id}` | Delete rule |
| PATCH | `/api/v1/admin/rules/{id}/status` | Change status (active/paused/archived) |
| POST | `/api/v1/admin/rules/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/rules/import` | Bulk import rules |
| GET | `/api/v1/admin/rules/export` | Bulk export rules |
| POST | `/api/v1/admin/connectors` | Create connector |
| GET | `/api/v1/admin/connectors` | List connectors |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| GET | `/api/v1/admin/engine/status` | Engine status |
| POST | `/api/v1/admin/engine/reload` | Hot-reload rules |

### Data API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/data/{channel}` | Process message synchronously |
| POST | `/api/v1/data/{channel}/async` | Submit for async processing (returns job ID) |
| GET | `/api/v1/data/jobs/{id}` | Poll async job result |
| POST | `/api/v1/data/batch` | Process a batch of messages |

### Operational

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Component health checks |
| GET | `/metrics` | Prometheus metrics |

All error responses follow a consistent structure:

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "Rule with id '...' not found"
  }
}
```

## Configuration

```toml
[server]
host = "0.0.0.0"
port = 8080
# workers = <num_cpus>         # Defaults to number of CPU cores

[storage]
path = "orion.db"              # SQLite database file path

[ingest]
max_payload_size = 1048576     # Maximum payload size in bytes (1 MB)
batch_size = 100               # Batch processing size

[engine]
max_concurrent_workflows = 100

[queue]
workers = 4                    # Concurrent async job workers
buffer_size = 1000             # Channel buffer for pending jobs

[kafka]                        # Requires the `kafka` feature flag
enabled = false
brokers = ["localhost:9092"]
group_id = "orion"

[logging]
level = "info"                 # trace, debug, info, warn, error
format = "pretty"              # pretty or json

[metrics]
enabled = false
```

Override any setting with environment variables using double-underscore nesting:

```bash
ORION_SERVER__PORT=9090
ORION_KAFKA__ENABLED=true
ORION_LOGGING__FORMAT=json
```

All settings have sensible defaults. You can run Orion with no config file at all — `cargo run` just works.

## Use Cases

- **Externalize pricing logic** — move discount rules, tax calculations, and tiered pricing out of your e-commerce app and into JSON rules you can update without deploying
- **Route and transform webhooks** — receive events from Stripe, GitHub, or Shopify, transform them, and route to internal services without writing glue code
- **Validate incoming payloads** — reject malformed data with clear error messages before it hits your database
- **Enrich events** — call external APIs to add context (user profiles, geo data, risk scores) before processing
- **Build internal automation** — create IFTTT-style rules that trigger actions across your backend systems
- **Bridge protocols** — REST-to-Kafka, Kafka-to-HTTP, and Kafka-to-Kafka routing with transformation
- **AI-managed business rules** — let LLMs create and update rules from natural language requirements via the REST API
- **LLM tool calling** — use Orion as a function-calling target for AI agents that need to evaluate business logic or trigger workflows
- **Multi-agent orchestration** — route each agent's output to a dedicated channel, with rules that coordinate cross-agent workflows

## Observability

### Prometheus Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `messages_total` | Counter | `channel`, `status` | Total messages processed |
| `message_duration_seconds` | Histogram | `channel` | Processing latency |
| `active_rules` | Gauge | — | Rules loaded in engine |
| `errors_total` | Counter | `type` | Errors encountered |

### Health Check

`GET /health` returns component-level status:

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "rules_loaded": 42,
  "components": {
    "database": "ok",
    "engine": "ok"
  }
}
```

Returns `200` when healthy, `503` when degraded.

### Logging

- Structured JSON or pretty-printed format (configurable)
- Tracing spans for request lifecycle
- Request ID propagation via `x-request-id` header

## Deployment

Orion ships as a **single binary** with an embedded SQLite database — no external dependencies to manage.

- **Standalone** — run directly on a VM or bare metal
- **Docker** — `docker build -t orion . && docker run -p 8080:8080 orion`
- **Sidecar** — deploy alongside your application in Kubernetes

The embedded SQLite storage means you're up and running immediately. For production, configure persistent volume mounts for the database file.

Orion supports graceful shutdown on `SIGTERM` and `SIGINT`, draining in-flight requests before exiting — 12-factor app compliant.

## Built With

- [dataflow-rs](https://github.com/codetiger/dataflow-rs) — workflow engine
- [datalogic-rs](https://github.com/codetiger/datalogic-rs) — JSONLogic implementation
- [Axum](https://github.com/tokio-rs/axum) — HTTP framework
- [SQLx](https://github.com/launchbadge/sqlx) — async SQLite
- [Tokio](https://tokio.rs) — async runtime

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
