# Install & First Service

Get Orion running on your machine in under a minute.

> Prefer point-and-click? [**The Console (Orion UI)**](../getting-started/console.md) walks
> the same zero-to-live-service flow entirely in the browser — with a demo video.

## Installation

Choose your preferred method:

**Homebrew** (macOS and Linux):

```bash
brew install GoPlasmatic/tap/orion-server
```

**Shell installer** (Linux/macOS):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
```

**PowerShell** (Windows):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.ps1 | iex"
```

**Docker:**

```bash
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest
```

**From source** (requires Rust 1.88+):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion
```

## First Run

Start Orion with default settings (SQLite, port 8080):

```bash
orion-server
```

Verify it's running:

```bash
curl -s http://localhost:8080/health
```

```json
{
  "status": "ok",
  "version": "0.2.0",
  "uptime_seconds": 5,
  "workflows_loaded": 0,
  "components": {
    "database": "ok",
    "engine": "ok"
  }
}
```

Swagger UI is available at [http://localhost:8080/docs](http://localhost:8080/docs).

## Configuration

Create a config file for custom settings:

```bash
orion-server -c config.toml
```

Or use environment variables for individual overrides:

```bash
ORION_SERVER__PORT=9090 \
ORION_LOGGING__FORMAT=json \
orion-server
```

Common configuration scenarios:

```bash
# Use PostgreSQL instead of SQLite
ORION_STORAGE__URL="postgres://user:pass@localhost/orion" orion-server

# Enable admin authentication
ORION_ADMIN_AUTH__ENABLED=true \
ORION_ADMIN_AUTH__API_KEYS="your-secret-key" \
orion-server

# Enable metrics and tracing
ORION_METRICS__ENABLED=true \
ORION_TRACING__ENABLED=true \
ORION_TRACING__OTLP_ENDPOINT="http://localhost:4317" \
orion-server
```

Validate a config file without starting the server:

```bash
orion-server validate-config -c config.toml
```

## Create Your First Service

> **In a hurry?** One command deploys a first service (workflow + channel,
> activated) against your running instance and sends a test request:
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh | bash
> ```
>
> The steps below build a service by hand so you can see each moving part.

**1. Create a workflow:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_id": "hello-world",
    "name": "Hello World",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse", "function": {
          "name": "parse_json", "input": { "source": "payload", "target": "req" }
      }},
      { "id": "greet", "name": "Greet", "function": {
          "name": "map", "input": { "mappings": [
            { "path": "data.req.greeting", "logic": {
              "cat": ["Hello, ", { "var": "data.req.name" }, "!"]
            }}
          ]}
      }}
    ]
  }'
```

**2. Activate the workflow:**

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/hello-world/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**3. Create and activate a channel:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "hello", "name": "hello", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/hello",
        "methods": ["POST"], "workflow_id": "hello-world" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/hello/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

**4. Test it:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/hello \
  -H "Content-Type: application/json" \
  -d '{ "data": { "name": "World" } }'
```

```json
{
  "status": "ok",
  "data": { "req": { "name": "World", "greeting": "Hello, World!" } },
  "errors": []
}
```

## Orion CLI

The [Orion CLI](https://github.com/GoPlasmatic/Orion-cli) provides a command-line interface and MCP server for managing Orion. No curl commands needed.

**Homebrew** (macOS and Linux):

```bash
brew install GoPlasmatic/tap/orion-cli
```

**Shell installer** (Linux/macOS):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion-cli/releases/latest/download/orion-cli-installer.sh | sh
```

**PowerShell** (Windows):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion-cli/releases/latest/download/orion-cli-installer.ps1 | iex"
```

**From source** (requires Rust 1.88+):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion-cli
```

**Usage:**

```bash
orion-cli config set-server http://localhost:8080
orion-cli health
orion-cli workflows list
orion-cli channels list
orion-cli send hello -d '{ "data": { "name": "World" } }'
```

The full lifecycle — create, activate, dry-run, then send live data — in one tool:

<div class="asciinema-player" data-cast="casts/cli-lifecycle.cast"></div>
<span class="asciinema-caption">▶ Click to play. Dry-run testing and live traffic flow through the same workflow.</span>

See the [CLI reference](https://github.com/GoPlasmatic/Orion-cli) for the full command list, or set up the [MCP Server](./mcp-setup.md) for AI assistant integration.

## Testing Workflows Offline

`orion-server` runs workflows without a server, a database or a network, so a
workflow can be developed and regression-tested the way any other code is.

Connector-backed tasks — `http_call`, `db_read`, `data_query`, `channel_call`,
and the rest — are answered from a **stub file** rather than a real backend:

```json
{
  "http_call":    { "crm": { "name": "Ada Lovelace" } },
  "data_query":   { "orders-db": [ { "id": 1, "total": 10 } ] },
  "channel_call": { "inventory-check": { "in_stock": true } }
}
```

The outer key is the function, the inner key is the task's `connector` (or its
`channel` for `channel_call`), and `"*"` matches any target.

```bash
orion-server dry-run -w workflow.json -i input.json --stubs stubs.json
```

A task with no matching stub **fails** and names the stub that would satisfy
it — a half-stubbed run reporting success would be worse than no stubs at all,
because it looks like a pass.

> This is the offline counterpart to `POST /workflows/{id}/test`, which runs the
> same workflow against **live** connectors: it will POST to real webhooks,
> write to real databases and publish to real topics. Reach for the endpoint
> when you mean to touch the real systems, and for `dry-run` when you do not.

### A regression suite

`orion-server test` runs a directory of cases and exits non-zero on any failure,
so a suite gates CI the way `lint`, `validate-config` and `preflight` do.

A case is a `*.case.json` file — the suffix is what separates cases from the
workflows and fixtures that live beside them:

```json
{
  "name": "flags high-value orders",
  "workflow": "high-value-order.json",
  "input": { "order_id": "ORD-1", "total": 25000 },
  "stubs": { "http_call": { "crm": { "name": "Ada" } } },
  "expect": {
    "data.order.flagged": true,
    "data.order.customer_name": "Ada"
  }
}
```

```bash
$ orion-server test ./workflow-tests
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

`workflow` and `stubs_file` are resolved relative to the case file. `expect`
maps dotted output paths to expected values (a leading `data.` is optional).
`expect_errors` lists expected task-error codes and defaults to empty — so a
workflow that starts failing its tasks cannot pass silently.

## Next Steps

- Connect a real database: [Your First Connector](../getting-started/first-connector.md)
- Browse the [API Reference](../api/admin.md) for all available endpoints
- Explore [Production Features](../features/observability.md) for observability, security, and resilience
- See the [Config Reference](../configuration/reference.md) for all configuration options
