<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **Stop building microservices for business logic.**

  New business logic shouldn't need a new service. Orion consolidates your business logic into a
  single platform with governance built in — observability, circuit breakers, hot-reload,
  and audit trails out of the box. Add logic as JSON rules, not as services.

  AI makes this even faster: LLMs generate rules, not infrastructure. Dry-run validates
  them before deploy. Orion handles the rest.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion)](https://github.com/GoPlasmatic/Orion/releases)
</div>

---

## Quick Start

**1. Install and start** — no config, no database to provision:

```bash
brew install GoPlasmatic/tap/orion   # or: curl installer, cargo install (see Install)
orion-server
```

**2. Tell AI what you need:**

```
"Flag orders over $10,000 for manual review with an alert message"
```

**3. AI generates the rule — create it via API:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{
    "name": "High-Value Order",
    "channel": "orders",
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
          "input": {
            "mappings": [
              { "path": "data.order.flagged", "logic": true },
              { "path": "data.order.alert", "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
            ]
          }
      }}
    ]
  }'
```

**4. Push data** — the pipeline fires automatically:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

**Response** — data is parsed and transformed:

```json
{
  "id": "...",
  "status": "ok",
  "data": {
    "order": {
      "order_id": "ORD-9182",
      "total": 25000,
      "flagged": true,
      "alert": "High-value order: $25000"
    }
  },
  "errors": []
}
```

You described what you needed. AI generated the rule. Your app pushed JSON — the engine handled the rest. Change the threshold from 10000 to 5000? Tell AI, one API call. No redeployment.

---

## Why Orion?

| Feature | Why it matters |
|---------|----------------|
| **One Platform, Not N Services** | Consolidate business logic into a single governed instance instead of building separate microservices |
| **Governance Built In** | Every rule gets observability, circuit breakers, retries, rate limiting, and versioning automatically |
| **AI-Ready** | JSON rules that LLMs can generate, validate, and deploy via API |
| **Single Binary** | No runtime dependencies — embedded SQLite with WAL mode |
| **Hot-Reload** | Update rules at runtime — zero downtime, in-flight requests complete on the old engine |
| **Dry-Run Testing** | [Test any rule](docs/api-reference.md#admin-api) with sample data and get a full execution trace |
| **JSONLogic Standard** | Portable, safe, language-agnostic rule conditions |
| **Task Pipelines** | Chain parsing, filtering, transforms, API calls, and publishing |
| **GitOps-Ready** | Custom rule IDs + [import/export](docs/production-features.md) for CI/CD workflows |
| **Rule Versioning** | Automatic [audit trail](docs/production-features.md#rule-versioning) for every rule change |
| **Connectors** | Named external service configs with [auth and retry](docs/connectors.md) — secrets stay out of rules |
| **Sync, Async & Batch** | Three [processing modes](docs/api-reference.md#data-api) for any workload |
| **Prometheus Metrics** | Built-in [counters, histograms, and health checks](docs/observability.md) |

## Before & After

**Before** — business logic sprawled across microservices:

```
pricing-service/      → 800 lines + Dockerfile + CI + health checks + metrics
fraud-service/        → 600 lines + Dockerfile + CI + health checks + metrics
routing-service/      → 400 lines + Dockerfile + CI + health checks + metrics
notification-service/ → 500 lines + Dockerfile + CI + health checks + metrics

4 services × (governance + deployment + monitoring) = operational burden
```

Every service needs its own health checks, metrics, error handling, and deployment pipeline. Every change requires a code review, a deploy, and a prayer.

**After** — tell AI what you need, logic lives in one governed platform:

```
One Orion instance:
  Channel "orders"   → pricing rules + fraud rules
  Channel "events"   → routing rules + notification rules

Governance? Already there. Metrics? Built in. Deploy? Already running.
```

Tell AI: *"Alert Slack when orders exceed $10K from accounts under 30 days old"* — it generates a rule like the Quick Start example above: parse, condition, action. One JSON object, one API call to deploy.

Change the threshold from 10000 to 5000? Tell AI. One API call. No restarts, no redeployments, no coordination.

## Three Processing Modes

```
Sync     POST /api/v1/data/{channel}         → Immediate response with transformed data
Async    POST /api/v1/data/{channel}/async   → Returns job_id, poll with GET /api/v1/data/jobs/{id}
Batch    POST /api/v1/data/batch             → Multiple messages in one request, per-message results
```

Choose the right mode for your workload. Sync for request-response flows, async for fire-and-forget, batch for bulk processing.

## Dry-Run Testing

Test any rule against sample data before activating it — with a full execution trace:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/rules/{id}/test \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

```json
{
  "matched": true,
  "trace": { "steps": [
    { "task_id": "parse", "result": "executed" },
    { "task_id": "flag",  "result": "executed" }
  ]},
  "output": { "order": { "order_id": "ORD-9182", "total": 25000, "flagged": true, "alert": "High-value order: $25000" } },
  "errors": []
}
```

See exactly which tasks ran, which were skipped, and what the output looks like — before a single real message flows through.

## AI-Ready by Design

**AI writes rules, not services.** That's the key insight. When AI generates a microservice, you still need to add governance — health checks, metrics, retries, error handling. When AI generates an Orion rule, governance is already there. The platform guarantees it.

Rule engines are the obvious choice for AI-driven development:

- **Constrained output format** — JSON rules that LLMs generate reliably, not open-ended code
- **Built-in validation** — dry-run tests catch errors before deploy
- **Platform-level governance** — AI can't skip observability, circuit breakers, or versioning
- **Safe rollback** — version history for every AI-generated rule change

**AI prompt:**

```
"Flag orders over $10,000 from accounts less than 30 days old for manual review.
Notify the fraud team via the slack-fraud connector."
```

**AI generates the complete rule:**

```json
{
  "name": "New Account Fraud Alert",
  "channel": "orders",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "order" }
    }},
    { "id": "flag", "name": "Flag Suspicious",
      "condition": { "and": [
        { ">": [{ "var": "data.order.total" }, 10000] },
        { "<": [{ "var": "data.order.account_age_days" }, 30] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.flagged", "logic": true },
        { "path": "data.order.review_reason", "logic": "high_value_new_account" }
      ]}}
    },
    { "id": "notify", "name": "Notify Fraud Team",
      "condition": { "==": [{ "var": "data.order.flagged" }, true] },
      "function": { "name": "http_call", "input": {
          "connector": "slack-fraud",
          "method": "POST",
          "body_logic": { "var": "data.order" }
      }}
    }
  ]
}
```

Send data the same way as the Quick Start — `POST /api/v1/data/orders` — and the pipeline flags, enriches, and notifies in one pass.

The full lifecycle — create, dry-run test, activate, update, delete — is available through the REST API. Every endpoint accepts and returns well-structured JSON, making Orion a natural fit for AI tool calling, MCP tools, or multi-agent orchestration. See [Use Cases & Patterns](docs/use-cases.md#ai-workflow--cicd) for prompt templates, validation workflows, and CI/CD patterns.

## How It Works

```
Inbound (REST or Kafka)
        │
        ▼
   Channel Router
   (route by topic: "orders", "events", "alerts")
        │
        ▼
   Task Pipeline
   (parse → filter → transform → enrich → call APIs → publish)
        │
        ▼
   Output (HTTP response / Kafka / webhooks)
```

## Built-in Task Functions

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context for downstream tasks |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields, constraints, and schema-like checks |
| `enrich` | Fetch external data via HTTP and merge it into the message |
| `http_call` | Invoke downstream APIs, webhooks, or services |
| `publish_json` | Serialize data to JSON output format |
| `publish_xml` | Serialize data to XML output format |
| `publish_kafka` | Publish messages to [Kafka topics](docs/kafka.md) |
| `log` | Emit structured log entries for auditing and debugging |

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

No database to provision. No runtime dependencies. Download, run, done.

## Observability

- **Prometheus metrics** at `GET /metrics` — `messages_total`, `message_duration_seconds`, `active_rules`, `errors_total` with per-channel labels
- **Health checks** at `GET /health` — component-level status (database, engine) with 200 OK or 503 degraded
- **Request ID propagation** — every request gets an `x-request-id` header for distributed tracing
- **Structured logging** — switch between `pretty` (dev) and `json` (production) formats, with `RUST_LOG` filtering

See [Observability](docs/observability.md) for details.

## Install

```bash
# macOS (Homebrew)
brew install GoPlasmatic/tap/orion

# macOS / Linux (shell installer)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-installer.ps1 | iex"

# From source
cargo install --git https://github.com/GoPlasmatic/Orion.git
```

Verify with `orion-server --version`. See [Configuration](docs/configuration.md#deployment) for Docker and deployment options.

## Performance

**100K+ requests/sec** on a single instance (Apple M2 Pro, release build, 50 concurrent connections):

| Scenario | Req/sec | Avg Latency | P99 Latency |
|----------|--------:|------------:|------------:|
| Simple rule (1 task) | 100,882 | 1.50 ms | 1.30 ms |
| Complex rule (5 tasks) | 85,768 | 1.50 ms | 1.60 ms |
| 12 rules on one channel | 90,023 | 1.50 ms | 1.60 ms |

Zero errors across all scenarios, including concurrent engine reloads. Run `./tests/benchmark/bench.sh` to reproduce.

- **Pre-compiled JSONLogic** — conditions compiled once at load time, not interpreted per request
- **Zero-downtime reloads** — in-flight requests complete on the old engine while the new one swaps in
- **Lock-free reads** — engine cloned behind an Arc, read lock released immediately
- **SQLite WAL mode** — concurrent reads and writes without blocking
- **Async-first runtime** — built on Tokio for non-blocking I/O across all operations

## Use Cases

- **Externalize pricing logic** — discount rules, tax calculations, and tiered pricing as JSON
- **Route and transform webhooks** — Stripe, GitHub, Shopify events to internal services
- **Enrich events** — call external APIs to add context before processing
- **Bridge protocols** — REST-to-Kafka, Kafka-to-HTTP routing with transformation
- **AI-managed business rules** — LLMs create and update rules via the REST API
- **Multi-agent orchestration** — route agent outputs to channels with coordinating rules

See [Use Cases & Patterns](docs/use-cases.md) for complete, tested examples of each pattern.

## Documentation

| Guide | Description |
|-------|-------------|
| [API Reference](docs/api-reference.md) | All endpoints, query parameters, and error format |
| [Configuration](docs/configuration.md) | Config file, env vars, deployment, and production checklist |
| [Connectors](docs/connectors.md) | Auth schemes, retry policies, and secret masking |
| [Kafka Integration](docs/kafka.md) | Topic mapping, metadata injection, DLQ, and publishing |
| [Production Features](docs/production-features.md) | Custom IDs, fault tolerance, tags, dynamic paths, versioning |
| [Use Cases & Patterns](docs/use-cases.md) | Tested examples, AI prompt templates, validation workflows, and CI/CD |
| [Observability](docs/observability.md) | Prometheus metrics, health checks, engine status, and logging |
| [CLI Tool](https://github.com/GoPlasmatic/Orion-cli) | Command-line tool for managing rules, connectors, and data |

## Built With

Built on battle-tested Rust libraries: [Axum](https://github.com/tokio-rs/axum) (HTTP), [Tokio](https://tokio.rs) (async runtime), [SQLx](https://github.com/launchbadge/sqlx) (database), and SQLite (storage). The rule engine is powered by [datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) (JSONLogic) and [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) (workflow orchestration).

## Contributing

Contributions are welcome! Please open an issue or submit a pull request on [GitHub](https://github.com/GoPlasmatic/Orion).

```bash
cargo build                  # Build
cargo test                   # Run tests
cargo clippy                 # Lint
cargo fmt                    # Format
```

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
