# CLI Setup

Get Orion running on your machine in under a minute.

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

**From source** (requires Rust 1.85+):

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
  "version": "0.1.0",
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
ORION_ADMIN_AUTH__API_KEY="your-secret-key" \
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

**1. Create a workflow:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
    "id": "hello-world",
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

**From source** (requires Rust 1.85+):

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

See the [CLI reference](https://github.com/GoPlasmatic/Orion-cli) for the full command list, or set up the [MCP Server](./mcp-setup.md) for AI assistant integration.

## Next Steps

- Browse the [API Reference](../api/admin.md) for all available endpoints
- Explore [Production Features](../features/observability.md) for observability, security, and resilience
- See the [Config Reference](../configuration/reference.md) for all configuration options
