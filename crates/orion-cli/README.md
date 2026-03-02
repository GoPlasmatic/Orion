<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **The command-line interface and MCP server for [Orion](https://github.com/GoPlasmatic/Orion) — manage rules, connectors, and data pipelines from your terminal or AI assistant.**

  Create, test, and deploy business rules. Send data through channels. Monitor engine health and metrics. Use as a CLI or as an MCP server for Claude Desktop, Cursor, and other AI tools.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion-cli)](https://github.com/GoPlasmatic/Orion-cli/releases)
</div>

---

## Quick Start

**1. Install the CLI:**

```bash
cargo install --git https://github.com/GoPlasmatic/Orion-cli.git
```

**2. Point it at your [Orion server](https://github.com/GoPlasmatic/Orion):**

```bash
orion config set-server http://localhost:8080
```

**3. Check the server is running:**

```bash
orion health
```

```
Orion Server v0.1.0
  Status:       OK
  Uptime:       2h 30m
  Rules loaded: 15
  Components:
    database     OK
    engine       OK
```

**4. Create a rule, test it, send data:**

```bash
# Create a rule from a JSON file
orion rules create -f high-value-order.json

# Dry-run test it with sample data
orion rules test <RULE_ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace

# Send real data through the channel
orion send orders -d '{"order_id":"ORD-9182","total":25000}'
```

---

## Commands

| Command | Description |
|---------|-------------|
| `health` | Check server health and component status |
| `rules` | Manage rules — create, update, delete, test, import/export, diff |
| `connectors` | Manage connectors — create, update, delete, enable/disable |
| `send` | Send data through channels (sync, async, or batch) |
| `traces` | View and monitor execution traces |
| `jobs` | Monitor async job status |
| `engine` | View engine status and trigger reloads |
| `metrics` | Retrieve Prometheus metrics |
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

## Rules Management

Full lifecycle management for [Orion rules](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md#admin-api):

```bash
# List rules with filters
orion rules list --channel orders --status active --tag fraud

# Get full rule details
orion rules get <ID>

# Create from file or inline JSON
orion rules create -f rule.json
orion rules create -d '{"name":"My Rule","channel":"orders",...}'

# Update a rule (version auto-increments)
orion rules update <ID> -f updated-rule.json

# Change rule status
orion rules activate <ID>
orion rules pause <ID>
orion rules archive <ID>

# Delete (with confirmation prompt)
orion rules delete <ID>
```

### Dry-Run Testing

Test any rule against sample data before activating — with a full execution trace:

```bash
orion rules test <ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace
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
# Export rules (with optional filters)
orion rules export --channel orders > rules.json

# Import rules from file
orion rules import -f rules.json

# Preview import without applying
orion rules import -f rules.json --dry-run

# Compare local file against server state
orion rules diff -f rules.json
```

The diff command shows color-coded changes: **+** new, **~** modified, **=** unchanged, **-** deleted.

---

## Connectors

Manage [named external service configurations](https://github.com/GoPlasmatic/Orion/blob/main/docs/connectors.md) with auth and retry policies:

```bash
orion connectors list
orion connectors get <ID>
orion connectors create -f connector.json
orion connectors update <ID> -f connector.json
orion connectors delete <ID>
orion connectors enable <ID>
orion connectors disable <ID>
```

---

## Sending Data

Three [processing modes](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md#data-api) for any workload:

### Synchronous (default)

```bash
orion send orders -d '{"order_id":"ORD-001","amount":150}'
```

### Asynchronous

```bash
# Fire and forget — returns job_id
orion send orders --async-mode -d '{"amount":100}'

# Submit and wait for completion
orion send orders --async-mode --wait --timeout 30 -d '{"amount":100}'
```

### Batch

```bash
orion send --batch -d '[{"order_id":"1","amount":50},{"order_id":"2","amount":75}]'
```

---

## Job Monitoring

Track async job status:

```bash
# Check job status
orion jobs get <JOB_ID>

# Poll until complete (with timeout)
orion jobs wait <JOB_ID> --interval 2 --timeout 60
```

Exit codes: `0` completed, `1` failed, `2` timeout.

---

## Engine Control

```bash
# View engine status — version, uptime, rule counts, channels
orion engine status

# Hot-reload rules (zero downtime)
orion engine reload
```

---

## MCP Server

Orion includes a built-in [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) server, enabling AI assistants like Claude Desktop and Cursor to manage your Orion instance directly.

### Stdio Transport (Claude Desktop / Cursor)

```bash
orion mcp serve --server http://localhost:8080
```

### HTTP Transport (Remote Clients)

```bash
orion mcp serve --http --server http://localhost:8080
orion mcp serve --http --bind 0.0.0.0:9090 --server http://localhost:8080
```

### Claude Desktop Configuration

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion",
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
    "command": "orion",
    "args": ["mcp", "serve"],
    "env": {
      "ORION_SERVER_URL": "http://localhost:8080"
    }
  }
}
```

### Available MCP Tools

The MCP server exposes 29 tools covering the full Orion API:

| Category | Tools |
|----------|-------|
| **Health** | `health_check` |
| **Engine** | `engine_status`, `engine_reload` |
| **Rules** | `rules_list`, `rules_get`, `rules_create`, `rules_update`, `rules_delete`, `rules_activate`, `rules_pause`, `rules_archive`, `rules_test`, `rules_validate`, `rules_rollout`, `rules_versions`, `rules_create_version`, `rules_export`, `rules_import` |
| **Connectors** | `connectors_list`, `connectors_get`, `connectors_create`, `connectors_update`, `connectors_delete`, `connectors_enable`, `connectors_disable` |
| **Data** | `data_send_sync`, `data_send_async` |
| **Traces** | `traces_list`, `traces_get` |
| **Metrics** | `get_metrics` |

---

## Output Formats

All commands support three output formats:

```bash
orion --output table rules list    # Pretty tables (default)
orion --output json  rules list    # JSON for scripting
orion --output yaml  rules list    # YAML for config files
```

Use `--quiet` for minimal output (just IDs) — ideal for shell scripts:

```bash
RULE_ID=$(orion --quiet rules create -f rule.json)
orion rules test "$RULE_ID" -d '{"data":{"amount":100}}'
```

---

## Configuration

Configuration is stored in `~/.orion/config.toml`:

```toml
server_url = "http://localhost:8080"
default_output = "table"
```

```bash
orion config set-server http://localhost:8080
orion config set default_output json
orion config show
```

**Precedence** (highest to lowest):
1. Command-line flags (`--server`, `--output`)
2. Environment variables (`ORION_SERVER_URL`, `NO_COLOR`)
3. Config file (`~/.orion/config.toml`)

---

## Shell Completions

```bash
# Bash
orion completions bash > ~/.bash_completions/orion

# Zsh
orion completions zsh > ~/.zfunctions/_orion

# Fish
orion completions fish > ~/.config/fish/completions/orion.fish
```

---

## Install

```bash
# From source
cargo install --git https://github.com/GoPlasmatic/Orion-cli.git
```

Requires Rust 1.85+.

---

## Related

- **[Orion Server](https://github.com/GoPlasmatic/Orion)** — The rules engine platform
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
