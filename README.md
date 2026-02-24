<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion Rule Engine

  **A lightweight, AI-ready rules engine. Single binary. Pure REST API. Zero dependencies.**

  Externalize business logic as JSON rules. Deploy as a standalone service or sidecar.
  Manage rules programmatically — from your code, your UI, or your AI agents.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![JSONLogic](https://img.shields.io/badge/JSONLogic-standard-green.svg)](https://jsonlogic.com)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion)](https://github.com/GoPlasmatic/Orion/releases)
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
| **CLI Tool** | Full-featured [`orion-cli`](docs/cli.md) for managing rules, connectors, and data |
| **Rule Versioning** | Automatic [audit trail](docs/production-features.md#rule-versioning) for every rule change |
| **Connectors** | Named external service configs with [auth and retry](docs/connectors.md) — secrets stay out of rules |
| **Sync, Async & Batch** | Three [processing modes](docs/api-reference.md#data-api) for any workload |
| **Prometheus Metrics** | Built-in [counters, histograms, and health checks](docs/observability.md) |

## The Problem

Business logic hardcoded in your application is painful:

- **Scattered if/else chains** — pricing rules, validation, routing decisions buried across services
- **Coupled to deployments** — changing a discount threshold means a full release cycle
- **Hard to audit** — no single place to see what rules exist or what they do
- **Impossible for non-devs to change** — every tweak requires a developer

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

Both `orion` (the server) and `orion-cli` (the management tool) are included. See [Configuration](docs/configuration.md#deployment) for Docker and deployment options.

## Quick Start

**1. Start the engine** — zero config needed:

```bash
orion
```

**2. Create a connector** — external services with built-in auth and retry ([more](docs/connectors.md)):

```bash
curl -X POST http://localhost:8080/api/v1/admin/connectors \
  -H "Content-Type: application/json" \
  -d '{ "name": "slack_webhook", "type": "http", "config": { "url": "https://hooks.slack.com/services" } }'
```

**3. Create a rule** — JSONLogic condition + task pipeline:

```bash
curl -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{
    "name": "High-Value Order Alert",
    "channel": "orders",
    "condition": { ">": [{ "var": "data.total" }, 10000] },
    "tasks": [
      { "id": "transform", "function": "map", "config": {
          "mappings": [{ "path": "data.alert", "logic": { "cat": ["Order $", { "var": "data.total" }] } }]
      }},
      { "id": "notify", "function": "http_call", "config": {
          "connector": "slack_webhook", "method": "POST", "path": "/T00/B00/XXX",
          "body": { "text": "{{ data.alert }}" }
      }}
    ]
  }'
```

**4. Push data** — the rule fires automatically:

```bash
curl -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

That's it. The rule matches, transforms the data, and sends a Slack notification. Your app just pushed JSON — the engine handled the rest. See the full [API Reference](docs/api-reference.md) for all endpoints.

## AI-Ready by Design

JSONLogic is a [well-known standard](https://jsonlogic.com) that LLMs already understand. AI agents can generate valid rules from natural language:

```
Prompt: "Flag orders over $10,000 from new customers for manual review"

Generated JSONLogic condition:
{ "and": [
  { ">": [{ "var": "data.total" }, 10000] },
  { "<": [{ "var": "data.account_age_days" }, 30] }
]}
```

The full lifecycle — create, [dry-run test](docs/api-reference.md#admin-api), activate, update, delete — is available through the REST API. Every endpoint accepts and returns well-structured JSON, making Orion a natural fit for AI tool calling, MCP tools, or multi-agent orchestration.

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

## JSONLogic

[JSONLogic](https://jsonlogic.com) is an open standard for expressing logic as JSON — portable, language-agnostic, and safe to execute:

```json
{ ">": [{ "var": "data.total" }, 10000] }
```

Rules are plain JSON: store them in any database, serialize over any transport, generate from UIs or AI agents. Orion pre-compiles conditions for near-zero evaluation overhead.

## Built-in Task Functions

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
| `publish_kafka` | Publish messages to [Kafka topics](docs/kafka.md) |
| `log` | Emit structured log entries for auditing and debugging |

## CLI

Manage everything from the terminal with `orion-cli`:

```bash
orion-cli rules list --output table        # List all rules
orion-cli rules diff -f rules.json         # Diff local vs live
orion-cli rules import -f rules.json       # Import rules
orion-cli send orders -d '{"data": {...}}' # Send data
orion-cli engine status                    # Check engine
```

See [CLI Reference](docs/cli.md) for the full command list and options.

## Use Cases

- **Externalize pricing logic** — discount rules, tax calculations, and tiered pricing as JSON
- **Route and transform webhooks** — Stripe, GitHub, Shopify events to internal services
- **Enrich events** — call external APIs to add context before processing
- **Bridge protocols** — REST-to-Kafka, Kafka-to-HTTP routing with transformation
- **AI-managed business rules** — LLMs create and update rules via the REST API
- **Multi-agent orchestration** — route agent outputs to channels with coordinating rules

## Performance

- **Pre-compiled JSONLogic** — conditions compiled once at load time, not interpreted per request
- **Zero-downtime reloads** — engine swap lets readers continue on the old engine while the new one loads
- **SQLite WAL mode** — concurrent reads and writes without blocking
- **Async-first runtime** — built on Tokio for non-blocking I/O across all operations

## Documentation

| Guide | Description |
|-------|-------------|
| [CLI Reference](docs/cli.md) | Full command reference for `orion-cli` |
| [API Reference](docs/api-reference.md) | All endpoints, query parameters, and error format |
| [Configuration](docs/configuration.md) | Config file, env vars, deployment, and production checklist |
| [Connectors](docs/connectors.md) | Auth schemes, retry policies, and secret masking |
| [Kafka Integration](docs/kafka.md) | Topic mapping, metadata injection, DLQ, and publishing |
| [Production Features](docs/production-features.md) | Custom IDs, fault tolerance, tags, dynamic paths, versioning |
| [Observability](docs/observability.md) | Prometheus metrics, health checks, engine status, and logging |

## Built With

- [dataflow-rs](https://github.com/codetiger/dataflow-rs) — workflow engine
- [datalogic-rs](https://github.com/codetiger/datalogic-rs) — JSONLogic implementation
- [Axum](https://github.com/tokio-rs/axum) — HTTP framework
- [SQLx](https://github.com/launchbadge/sqlx) — async SQLite
- [Tokio](https://tokio.rs) — async runtime

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
