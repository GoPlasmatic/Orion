<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion Rule Engine

  **Externalize your business logic in 5 minutes. Single binary. Zero dependencies. Just JSON.**

  Deploy as a standalone service or sidecar. Define rules as JSON. Push data via REST or Kafka.
  Change rules at runtime — no restarts, no redeployments, no downtime.

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
orion
```

**2. Create a rule** — parse payload, filter by condition, transform the result:

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
      { "id": "check", "name": "Check value", "function": {
          "name": "filter",
          "input": { "condition": { ">": [{ "var": "data.order.total" }, 10000] } }
      }},
      { "id": "flag", "name": "Flag order", "function": {
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

**3. Push data** — the pipeline fires automatically:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

**Response** — data is parsed, filtered, and transformed:

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

Your app just pushed JSON — the engine handled the rest. Change the threshold from 10000 to 5000? One API call. No redeployment.

---

## Why Orion?

| Feature | Why it matters |
|---------|----------------|
| **AI-Ready** | JSON rules that LLMs can generate, validate, and deploy via API |
| **Single Binary** | No runtime dependencies — embedded SQLite with WAL mode |
| **Hot-Reload** | Update rules at runtime — zero downtime, in-flight requests complete on the old engine |
| **Dry-Run Testing** | [Test any rule](docs/api-reference.md#admin-api) with sample data and get a full execution trace |
| **JSONLogic Standard** | Portable, safe, language-agnostic rule conditions |
| **Task Pipelines** | Chain parsing, filtering, transforms, API calls, and publishing |
| **GitOps-Ready** | Custom rule IDs + [import/export](docs/production-features.md) + CLI diff for CI/CD workflows |
| **Rule Versioning** | Automatic [audit trail](docs/production-features.md#rule-versioning) for every rule change |
| **Connectors** | Named external service configs with [auth and retry](docs/connectors.md) — secrets stay out of rules |
| **Sync, Async & Batch** | Three [processing modes](docs/api-reference.md#data-api) for any workload |
| **CLI Tool** | Full-featured [`orion-cli`](docs/cli.md) for managing rules, connectors, and data |
| **Prometheus Metrics** | Built-in [counters, histograms, and health checks](docs/observability.md) |

## Before & After

**Before** — business logic scattered across your codebase:

```python
# pricing.py
if order.total > 10000 and customer.age_days < 30:
    send_slack("New high-value customer!")
    flag_for_review(order)
elif order.total > 50000:
    send_slack("Whale alert!")
    notify_sales_team(order)
# ... 200 more lines across 12 files
```

Every change requires a code review, a deploy, and a prayer.

**After** — rules live outside your code:

```json
{
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Load data", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "order" }
    }},
    { "id": "check", "name": "Filter", "function": {
        "name": "filter", "input": {
          "condition": { "and": [
            { ">": [{ "var": "data.order.total" }, 10000] },
            { "<": [{ "var": "data.order.account_age_days" }, 30] }
          ]}
        }
    }},
    { "id": "notify", "name": "Alert", "function": {
        "name": "http_call", "input": { "connector": "slack", "method": "POST" }
    }}
  ]
}
```

Change the threshold from 10000 to 5000? One API call. Add a new rule? Another API call. No restarts, no redeployments, no coordination.

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
  "trace": {
    "steps": [
      { "task_id": "parse", "result": "executed" },
      { "task_id": "check", "result": "executed" },
      { "task_id": "flag",  "result": "executed" }
    ]
  },
  "output": {
    "order": { "order_id": "ORD-9182", "total": 25000, "flagged": true, "alert": "High-value order: $25000" }
  },
  "errors": []
}
```

See exactly which tasks ran, which were skipped, and what the output looks like — before a single real message flows through.

## AI-Ready by Design

JSONLogic is a [well-known standard](https://jsonlogic.com) that LLMs already understand. AI agents can generate valid rules from natural language:

```
Prompt: "Flag orders over $10,000 from new customers for manual review"

Generated JSONLogic filter condition:
{ "and": [
  { ">": [{ "var": "data.order.total" }, 10000] },
  { "<": [{ "var": "data.order.account_age_days" }, 30] }
]}
```

The full lifecycle — create, dry-run test, activate, update, delete — is available through the REST API. Every endpoint accepts and returns well-structured JSON, making Orion a natural fit for AI tool calling, MCP tools, or multi-agent orchestration.

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

## GitOps & CLI

Manage everything from the terminal with `orion-cli` — like `kubectl` for your rules engine:

```bash
orion-cli rules list --output table        # List all rules
orion-cli rules diff -f rules.json         # Diff local vs live
orion-cli rules import -f rules.json       # Import rules from file
orion-cli rules test <id> -d '{"data": {"total": 25000}}' --trace  # Test with trace
orion-cli send orders -d '{"data": {...}}' # Send data
orion-cli engine status                    # Check engine health
orion-cli completions zsh                  # Shell completions
```

**GitOps workflow:** Store rules as JSON in your repo. Diff against live state. Deploy from CI/CD:

```bash
orion-cli rules diff -f rules.json         # Preview changes: + new, ~ modified, - deleted
orion-cli rules import -f rules.json       # Apply — partial failures don't roll back successful imports
```

Custom rule IDs (e.g., `high-value-order-alert` instead of UUIDs) make rules trackable across environments. See [CLI Reference](docs/cli.md) for the full command list.

## Deploy Anywhere

```
┌────────────────┐   ┌────────────────────┐   ┌────────────────┐
│   Standalone    │   │      Sidecar       │   │     Docker      │
│                 │   │                    │   │                 │
│   ./orion       │   │  ┌─────┐ ┌──────┐ │   │  docker run \   │
│                 │   │  │ App │─│Orion │ │   │   orion:latest  │
│   That's it.    │   │  └─────┘ └──────┘ │   │                 │
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

Both `orion` (the server) and `orion-cli` (the management tool) are included. Verify with `orion --version`. See [Configuration](docs/configuration.md#deployment) for Docker and deployment options.

## Performance

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

## Documentation

| Guide | Description |
|-------|-------------|
| [API Reference](docs/api-reference.md) | All endpoints, query parameters, and error format |
| [CLI Reference](docs/cli.md) | Full command reference for `orion-cli` |
| [Configuration](docs/configuration.md) | Config file, env vars, deployment, and production checklist |
| [Connectors](docs/connectors.md) | Auth schemes, retry policies, and secret masking |
| [Kafka Integration](docs/kafka.md) | Topic mapping, metadata injection, DLQ, and publishing |
| [Production Features](docs/production-features.md) | Custom IDs, fault tolerance, tags, dynamic paths, versioning |
| [Observability](docs/observability.md) | Prometheus metrics, health checks, engine status, and logging |

## Built With

Built on battle-tested Rust libraries: [Axum](https://github.com/tokio-rs/axum) (HTTP), [Tokio](https://tokio.rs) (async runtime), [SQLx](https://github.com/launchbadge/sqlx) (database), and SQLite (storage). The rule engine is powered by [datalogic-rs](https://github.com/codetiger/datalogic-rs) (JSONLogic) and [dataflow-rs](https://github.com/codetiger/dataflow-rs) (workflow orchestration).

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
