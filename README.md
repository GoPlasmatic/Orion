<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion Rule Engine

  **A lightweight, JSONLogic-based rules engine you deploy as a service.**
  Single binary. REST API. Any language.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
</div>

---

Orion Rule Engine lets you externalize business logic from your applications. Define rules as JSON using the [JSONLogic](https://jsonlogic.com) standard, manage them through a REST API, and push data from any language or framework. Rules evaluate instantly, trigger task pipelines, and can be changed at runtime — zero redeployment required.

## The Problem

Business logic hardcoded in your application is painful:

- **Scattered if/else chains** — pricing rules, validation, routing decisions buried across services
- **Coupled to deployments** — changing a discount threshold means a full release cycle
- **Hard to audit** — no single place to see what rules exist or what they do
- **Impossible for non-devs to change** — every tweak requires a developer
- **Duplicated across languages** — the same logic reimplemented in every service

## The Solution

Deploy the rule engine as a sidecar or standalone service. Define rules as JSON with JSONLogic conditions. Push data via REST or Kafka. Rules evaluate instantly, trigger actions, and return results. Change rules at runtime — no restarts, no redeployments.

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

## Key Capabilities

- **Hot-reloadable rules** — add, update, or disable rules at runtime without restarts
- **Channel-based routing** — organize rules by domain (multi-tenant, multi-topic)
- **Rich task pipelines** — chain transforms, validations, API calls, and publishing steps
- **Connector system** — named configurations for external services; credentials stay out of rule definitions
- **Sync & async processing** — get results immediately or submit jobs and poll later
- **Batch processing** — process arrays of messages in a single request
- **Kafka integration** — consume from and produce to Kafka topics for event-driven architectures
- **Observability** — Prometheus metrics, health checks, structured JSON logging, execution tracing
- **Rule versioning** — every update creates a new version; roll back anytime
- **Dry-run testing** — test rules against sample payloads before activating
- **Import/export** — bulk move rules between environments as JSON

## Quick Start

### 1. Run the Engine

```bash
# From source
cargo run -- --config ./config.toml

# Or with Docker
docker run -p 8080:8080 orion/rule-engine
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

## JSONLogic

[JSONLogic](https://jsonlogic.com) is an open standard for expressing logic as JSON. It's portable, language-agnostic, and safe to execute.

```json
{ ">": [{ "var": "data.total" }, 10000] }
```

This reads: *"is `data.total` greater than 10,000?"*

JSONLogic supports arithmetic, string operations, array operations, boolean logic, and variable access. Because rules are plain JSON, they can be:

- **Stored** in any database
- **Serialized** over any transport
- **Generated** by UIs, scripts, or other services
- **Evaluated** without compiling or interpreting code

Orion Rule Engine pre-compiles JSONLogic conditions for near-zero evaluation overhead at runtime.

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

## API Reference

### Admin API

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/rules` | Create rule |
| GET | `/api/v1/admin/rules` | List and filter rules |
| GET | `/api/v1/admin/rules/{id}` | Get rule by ID |
| PUT | `/api/v1/admin/rules/{id}` | Update rule (creates new version) |
| PATCH | `/api/v1/admin/rules/{id}/status` | Change status (active/paused/archived) |
| POST | `/api/v1/admin/rules/{id}/test` | Dry-run on sample payload |
| POST | `/api/v1/admin/rules/import` | Bulk import rules |
| GET | `/api/v1/admin/rules/export` | Bulk export rules |
| POST | `/api/v1/admin/connectors` | Create connector |
| GET | `/api/v1/admin/connectors` | List connectors |
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

## Configuration

```toml
[server]
host = "0.0.0.0"
port = 8080

[storage]
driver = "sqlite"
path = "./orion.db"

[ingest.http]
enabled = true

[ingest.kafka]
enabled = false
brokers = ["localhost:9092"]
group_id = "orion-engine"

[engine]
workers = 4

[logging]
level = "info"
format = "json"

[metrics]
enabled = true
endpoint = "/metrics"
```

Override any setting with environment variables using double-underscore nesting:

```bash
ORION_SERVER__PORT=9090
ORION_INGEST__KAFKA__ENABLED=true
```

## Use Cases

- **Externalize pricing logic** — move discount rules, tax calculations, and tiered pricing out of your e-commerce app and into JSON rules you can update without deploying
- **Route and transform webhooks** — receive events from Stripe, GitHub, or Shopify, transform them, and route to internal services without writing glue code
- **Validate incoming payloads** — reject malformed data with clear error messages before it hits your database
- **Enrich events** — call external APIs to add context (user profiles, geo data, risk scores) before processing
- **Build internal automation** — create IFTTT-style rules that trigger actions across your backend systems
- **Bridge protocols** — REST-to-Kafka, Kafka-to-HTTP, and Kafka-to-Kafka routing with transformation

## Deployment

Orion Rule Engine ships as a **single binary** with an embedded SQLite database — no external dependencies to manage.

- **Standalone** — run directly on a VM or bare metal
- **Docker** — `docker run -p 8080:8080 orion/rule-engine`
- **Sidecar** — deploy alongside your application in Kubernetes

The embedded SQLite storage means you're up and running immediately. For production, configure persistent volume mounts for the database file.

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.

---

<div align="center">

Built by [Orion](https://github.com/ArkMindAI) | Powered by [Orion](https://github.com/ArkMindAI/Orion)

</div>
