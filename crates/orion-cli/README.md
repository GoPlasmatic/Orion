<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **The command-line interface and MCP server for [Orion](https://github.com/GoPlasmatic/Orion) — manage workflows, channels, connectors, and data pipelines from your terminal or AI assistant.**

  Create, test, and deploy workflows. Define channels as service endpoints. Send data through channels. Monitor engine health and metrics. Use as a CLI or as an MCP server for Claude Desktop, Cursor, and other AI tools.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion-cli)](https://github.com/GoPlasmatic/Orion-cli/releases)
</div>

---

## Quick Start

**1. Install the CLI:**

```bash
brew install GoPlasmatic/tap/orion-cli   # or: curl installer, cargo install (see Install)
```

**2. Point it at your [Orion server](https://github.com/GoPlasmatic/Orion):**

```bash
orion-cli config set-server http://localhost:8080
```

**3. Check the server is running:**

```bash
orion-cli health
```

```
Orion Server v0.1.0
  Status:       OK
  Uptime:       2h 30m
  Components:
    database     OK
    engine       OK
```

**4. Create a workflow and channel, test it, send data:**

```bash
# Create a workflow from a JSON file
orion-cli workflows create -f high-value-order.json

# Activate it
orion-cli workflows activate <WORKFLOW_ID>

# Create a channel that links to the workflow
orion-cli channels create -d '{"name":"orders","channel_type":"sync","protocol":"http","workflow_id":"<WORKFLOW_ID>"}'
orion-cli channels activate <CHANNEL_ID>

# Reload the engine to pick up changes
orion-cli engine reload

# Dry-run test with sample data
orion-cli workflows test <WORKFLOW_ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace

# Send real data through the channel
orion-cli send orders -d '{"order_id":"ORD-9182","total":25000}'
```

---

## Commands

| Command | Description |
|---------|-------------|
| `health` | Check server health and component status |
| `workflows` | Manage workflows — create, update, delete, test, import/export, diff |
| `channels` | Manage channels — create, update, delete, activate/archive, versioning, bulk import |
| `connectors` | Manage connectors — create, update, delete, enable/disable, circuit breakers, bulk import |
| `send` | Send data through channels (sync or async; `--profile` for timing breakdown) |
| `traces` | View and monitor execution traces |
| `engine` | View engine status and trigger reloads |
| `functions` | Inspect workflow task functions registered in the engine |
| `metrics` | Retrieve Prometheus metrics |
| `audit-logs` | View audit logs of admin actions |
| `backups` | Create and list database backups (SQLite) |
| `config` | Configure server URL and defaults |
| `completions` | Generate shell completions (bash, zsh, fish, powershell) |
| `mcp` | Start MCP server for AI tool integration |

### Global Flags

```
--server <URL>      Orion server URL (overrides config; env: ORION_SERVER_URL)
--output <FORMAT>   Output format: table, json, yaml (default: table)
--quiet             Suppress output, print only IDs or minimal info
--verbose           Show full response bodies and extra details
--no-color          Disable colored output (env: NO_COLOR)
--yes               Skip confirmation prompts
```

---

## Workflow Management

Full lifecycle management for [Orion workflows](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md#admin-api):

```bash
# List workflows with filters
orion-cli workflows list --status active --tag fraud

# Get full workflow details
orion-cli workflows get <ID>

# Create from file or inline JSON
orion-cli workflows create -f workflow.json
orion-cli workflows create -d '{"name":"My Workflow",...}'

# Create with a custom ID
orion-cli workflows create --id my-custom-id -f workflow.json

# Update a workflow (version auto-increments)
orion-cli workflows update <ID> -f updated-workflow.json

# Change workflow status
orion-cli workflows activate <ID>
orion-cli workflows archive <ID>

# Control rollout percentage
orion-cli workflows rollout <ID> -p 50

# Delete (with confirmation prompt)
orion-cli workflows delete <ID>
```

### Dry-Run Testing

Test any workflow against sample data before activating — with a full execution trace:

```bash
orion-cli workflows test <ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace
```

```
Result: MATCHED

Trace:
  parse    executed
  flag     executed

Output:
  {
    "order": {
      "order_id": "ORD-9182",
      "total": 25000,
      "flagged": true,
      "alert": "High-value order: $25000"
    }
  }
```

Supports input from file (`-f`), inline JSON (`-d`), or stdin (`--stdin`).

### Import, Export & Diff

GitOps-ready workflows for CI/CD pipelines:

```bash
# Export workflows (with optional filters)
orion-cli workflows export --status active > workflows.json

# Import workflows from file
orion-cli workflows import -f workflows.json

# Validate the import on the server without applying (reports would_create/would_fail)
orion-cli workflows import -f workflows.json --dry-run

# Compare local file against server state
orion-cli workflows diff -f workflows.json
```

The diff command shows color-coded changes: **+** new, **~** modified, **=** unchanged, **-** deleted.

---

## Channel Management

Channels are service endpoints that receive data and route it to workflows:

```bash
# List channels
orion-cli channels list --status active --protocol rest

# Create a channel
orion-cli channels create -d '{"name":"orders","channel_type":"sync","protocol":"rest","route_pattern":"/orders/{id}","workflow_id":"process-orders"}'

# Activate / Archive
orion-cli channels activate <ID>
orion-cli channels archive <ID>

# Version management
orion-cli channels versions <ID>
orion-cli channels new-version <ID>

# Bulk import (server-side validation with --dry-run)
orion-cli channels import -f channels.json --dry-run
orion-cli channels import -f channels.json
```

---

## Connectors

Manage [named external service configurations](https://github.com/GoPlasmatic/Orion/blob/main/docs/connectors.md) with auth and retry policies:

```bash
orion-cli connectors list
orion-cli connectors get <ID>
orion-cli connectors create -f connector.json
orion-cli connectors update <ID> -f connector.json
orion-cli connectors delete <ID>
orion-cli connectors enable <ID>
orion-cli connectors disable <ID>

# Circuit breaker management
orion-cli connectors circuit-breakers
orion-cli connectors reset-breaker <KEY>

# Bulk import (server-side validation with --dry-run)
orion-cli connectors import -f connectors.json --dry-run
orion-cli connectors import -f connectors.json
```

---

## Sending Data

[Processing modes](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md#data-api) for any workload:

### Synchronous (default)

```bash
orion-cli send orders -d '{"order_id":"ORD-001","amount":150}'

# Include a server-side execution profile (timing breakdown by phase/handler).
# Requires tracing.debug_profile_enabled on the server.
orion-cli send orders -d '{"order_id":"ORD-001","amount":150}' --profile
```

### Asynchronous

```bash
# Fire and forget — returns trace_id
orion-cli send orders --async-mode -d '{"amount":100}'

# Submit and wait for completion
orion-cli send orders --async-mode --wait --timeout 30 -d '{"amount":100}'
```

---

## Traces

View and monitor execution traces:

```bash
# Check trace status
orion-cli traces get <TRACE_ID>

# Poll until complete (with timeout)
orion-cli traces wait <TRACE_ID> --interval 2 --timeout 60
```

Exit codes: `0` completed, `1` failed, `2` timeout.

---

## Engine Control

```bash
# View engine status — version, uptime, workflow counts, channels
orion-cli engine status

# Hot-reload workflows and channels (zero downtime)
orion-cli engine reload
```

---

## Functions

Inspect the workflow task functions registered in the engine, with their input schemas:

```bash
# List functions (table view)
orion-cli functions list

# Full input schemas as JSON
orion-cli --output json functions list
```

---

## MCP Server

Orion includes a built-in [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) server, enabling AI assistants like Claude Desktop and Cursor to manage your Orion instance directly.

### Stdio Transport (Claude Desktop / Cursor)

```bash
orion-cli mcp serve --server http://localhost:8080
```

### HTTP Transport (Remote Clients)

```bash
orion-cli mcp serve --http --server http://localhost:8080
orion-cli mcp serve --http --bind 0.0.0.0:9090 --server http://localhost:8080
```

### Claude Desktop Configuration

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion-cli",
      "args": ["mcp", "serve"],
      "env": {
        "ORION_SERVER_URL": "http://localhost:8080"
      }
    }
  }
}
```

### Cursor Configuration

Add to Cursor MCP settings (Settings > MCP Servers):

```json
{
  "orion": {
    "command": "orion-cli",
    "args": ["mcp", "serve"],
    "env": {
      "ORION_SERVER_URL": "http://localhost:8080"
    }
  }
}
```

### Available MCP Tools

The MCP server exposes 45 tools covering the full Orion API:

| Category | Tools |
|----------|-------|
| **Health** | `health_check` |
| **Engine** | `engine_status`, `engine_reload` |
| **Workflows** | `workflows_list`, `workflows_get`, `workflows_create`, `workflows_update`, `workflows_delete`, `workflows_activate`, `workflows_archive`, `workflows_test`, `workflows_validate`, `workflows_rollout`, `workflows_versions`, `workflows_create_version`, `workflows_export`, `workflows_import` |
| **Channels** | `channels_list`, `channels_get`, `channels_create`, `channels_update`, `channels_delete`, `channels_activate`, `channels_archive`, `channels_versions`, `channels_create_version`, `channels_import` |
| **Connectors** | `connectors_list`, `connectors_get`, `connectors_create`, `connectors_update`, `connectors_delete`, `connectors_enable`, `connectors_disable`, `connectors_import` |
| **Circuit Breakers** | `circuit_breakers_list`, `circuit_breaker_reset` |
| **Data** | `data_send_sync`, `data_send_async` |
| **Traces** | `traces_list`, `traces_get` |
| **Functions** | `functions_list` |
| **Audit Logs** | `audit_logs_list` |
| **Backups** | `backups_create`, `backups_list` |
| **Metrics** | `get_metrics` |

---

## Output Formats

All commands support three output formats:

```bash
orion-cli --output table workflows list    # Pretty tables (default)
orion-cli --output json  workflows list    # JSON for scripting
orion-cli --output yaml  workflows list    # YAML for config files
```

Use `--quiet` for minimal output (just IDs) — ideal for shell scripts:

```bash
WF_ID=$(orion-cli --quiet workflows create -f workflow.json)
orion-cli workflows test "$WF_ID" -d '{"data":{"amount":100}}'
```

---

## Configuration

Configuration is stored in `~/.orion/config.toml`:

```toml
server_url = "http://localhost:8080"
default_output = "table"
```

```bash
orion-cli config set-server http://localhost:8080
orion-cli config set default_output json
orion-cli config show
```

**Precedence** (highest to lowest):
1. Command-line flags (`--server`, `--output`)
2. Environment variables (`ORION_SERVER_URL`, `NO_COLOR`)
3. Config file (`~/.orion/config.toml`)

---

## Shell Completions

```bash
# Bash
orion-cli completions bash > ~/.bash_completions/orion-cli

# Zsh
orion-cli completions zsh > ~/.zfunctions/_orion-cli

# Fish
orion-cli completions fish > ~/.config/fish/completions/orion-cli.fish
```

---

## Install

```bash
# Docker (MCP server mode)
docker run -p 8081:8081 ghcr.io/goplasmatic/orion-cli:latest mcp serve --http

# macOS (Homebrew)
brew install GoPlasmatic/tap/orion-cli

# macOS / Linux (shell installer)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion-cli/releases/latest/download/orion-cli-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion-cli/releases/latest/download/orion-cli-installer.ps1 | iex"

# From crates.io
cargo install orion-cli

# From source
cargo install --git https://github.com/GoPlasmatic/Orion-cli.git
```

Verify with `orion-cli --version`. Requires Rust 1.85+ for source builds.

---

## Related

- **[Orion Server](https://github.com/GoPlasmatic/Orion)** — The services runtime platform
- **[API Reference](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md)** — Full REST API documentation
- **[Connectors Guide](https://github.com/GoPlasmatic/Orion/blob/main/docs/connectors.md)** — Auth schemes, retry policies, and secrets
- **[Production Features](https://github.com/GoPlasmatic/Orion/blob/main/docs/production-features.md)** — Custom IDs, versioning, fault tolerance
- **[Use Cases & Patterns](https://github.com/GoPlasmatic/Orion/blob/main/docs/use-cases.md)** — Real-world examples and AI prompt templates
- **[Observability](https://github.com/GoPlasmatic/Orion/blob/main/docs/observability.md)** — Prometheus metrics, health checks, logging

## Contributing

Contributions are welcome! Please open an issue or submit a pull request on [GitHub](https://github.com/GoPlasmatic/Orion-cli).

```bash
cargo build          # Build
cargo test           # Run tests
cargo clippy         # Lint
cargo fmt            # Format
```

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.
