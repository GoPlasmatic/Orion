<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion CLI

  **The command-line interface for [Orion](https://github.com/GoPlasmatic/Orion) — manage rules, connectors, and data pipelines from your terminal.**

  Create, test, and deploy business rules. Send data through channels. Monitor engine health and metrics. All without leaving the command line.

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
  Rules loaded: 15
  Components:
    database     OK
    engine       OK
```

**4. Create a rule, test it, send data:**

```bash
# Create a rule from a JSON file
orion-cli rules create -f high-value-order.json

# Dry-run test it with sample data
orion-cli rules test <RULE_ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace

# Send real data through the channel
orion-cli send orders -d '{"order_id":"ORD-9182","total":25000}'
```

---

## Commands

| Command | Description |
|---------|-------------|
| `health` | Check server health and component status |
| `rules` | Manage rules — create, update, delete, test, import/export, diff |
| `connectors` | Manage connectors — create, update, delete, enable/disable |
| `send` | Send data through channels (sync, async, or batch) |
| `jobs` | Monitor async job status |
| `engine` | View engine status and trigger reloads |
| `metrics` | Retrieve Prometheus metrics |
| `config` | Configure server URL and defaults |
| `completions` | Generate shell completions (bash, zsh, fish, powershell) |

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
orion-cli rules list --channel orders --status active --tag fraud

# Get full rule details
orion-cli rules get <ID>

# Create from file or inline JSON
orion-cli rules create -f rule.json
orion-cli rules create -d '{"name":"My Rule","channel":"orders",...}'

# Update a rule (version auto-increments)
orion-cli rules update <ID> -f updated-rule.json

# Change rule status
orion-cli rules activate <ID>
orion-cli rules pause <ID>
orion-cli rules archive <ID>

# Delete (with confirmation prompt)
orion-cli rules delete <ID>
```

### Dry-Run Testing

Test any rule against sample data before activating — with a full execution trace:

```bash
orion-cli rules test <ID> -d '{"data":{"order_id":"ORD-9182","total":25000}}' --trace
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
orion-cli rules export --channel orders > rules.json

# Import rules from file
orion-cli rules import -f rules.json

# Preview import without applying
orion-cli rules import -f rules.json --dry-run

# Compare local file against server state
orion-cli rules diff -f rules.json
```

The diff command shows color-coded changes: **+** new, **~** modified, **=** unchanged, **-** deleted.

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
```

---

## Sending Data

Three [processing modes](https://github.com/GoPlasmatic/Orion/blob/main/docs/api-reference.md#data-api) for any workload:

### Synchronous (default)

```bash
orion-cli send orders -d '{"order_id":"ORD-001","amount":150}'
```

### Asynchronous

```bash
# Fire and forget — returns job_id
orion-cli send orders --async-mode -d '{"amount":100}'

# Submit and wait for completion
orion-cli send orders --async-mode --wait --timeout 30 -d '{"amount":100}'
```

### Batch

```bash
orion-cli send --batch -d '[{"order_id":"1","amount":50},{"order_id":"2","amount":75}]'
```

---

## Job Monitoring

Track async job status:

```bash
# Check job status
orion-cli jobs get <JOB_ID>

# Poll until complete (with timeout)
orion-cli jobs wait <JOB_ID> --interval 2 --timeout 60
```

Exit codes: `0` completed, `1` failed, `2` timeout.

---

## Engine Control

```bash
# View engine status — version, uptime, rule counts, channels
orion-cli engine status

# Hot-reload rules (zero downtime)
orion-cli engine reload
```

---

## Output Formats

All commands support three output formats:

```bash
orion-cli --output table rules list    # Pretty tables (default)
orion-cli --output json  rules list    # JSON for scripting
orion-cli --output yaml  rules list    # YAML for config files
```

Use `--quiet` for minimal output (just IDs) — ideal for shell scripts:

```bash
RULE_ID=$(orion-cli --quiet rules create -f rule.json)
orion-cli rules test "$RULE_ID" -d '{"data":{"amount":100}}'
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
