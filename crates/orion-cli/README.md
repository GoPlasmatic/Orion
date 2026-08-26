<div align="center">
  <img src="https://avatars.githubusercontent.com/u/207296579?s=200&v=4" alt="Orion Logo" width="120" height="120">

  # Orion

  **The command-line interface for [Orion](https://github.com/GoPlasmatic/Orion) — manage workflows, channels, connectors, and data pipelines from your terminal.**

  Create, test, and deploy workflows. Define channels as service endpoints. Send data through channels. Monitor engine health and metrics. Pair it with the [Orion agent skill](../../skills/orion/) to let an AI assistant drive the same commands.

  [![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
  [![Rust](https://img.shields.io/badge/rust-1.88+-orange.svg)](https://www.rust-lang.org)
  [![GitHub Release](https://img.shields.io/github/v/release/GoPlasmatic/Orion?filter=orion-cli-v*)](https://github.com/GoPlasmatic/Orion/releases)
</div>

---

## Quick Start

<div align="center">
  <img src="https://raw.githubusercontent.com/GoPlasmatic/Orion/main/docs/media/cli-lifecycle.gif" alt="orion-cli creating and activating a workflow, dry-running it, then sending live data" width="100%">
  <br>
  <em>Create, activate, dry-run and send — the full lifecycle from one terminal.</em>
</div>

**1. Install the CLI:**

```bash
brew install GoPlasmatic/tap/orion-cli   # or: curl installer, Docker (see Install)
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
Orion Server v1.0.0
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
orion-cli channels create -d '{"name":"orders","channel_type":"sync","protocol":"http","route_pattern":"/orders","methods":["POST"],"workflow_id":"<WORKFLOW_ID>"}'
orion-cli channels activate <CHANNEL_ID>

# Activation hot-reloads the engine on its own — no reload step needed

# Dry-run test with sample data
orion-cli workflows test <WORKFLOW_ID> -d '{"order_id":"ORD-9182","total":25000}' --trace

# Send real data through the channel
orion-cli send orders -d '{"order_id":"ORD-9182","total":25000}'
```

---

## Commands

| Command | Description |
|---------|-------------|
| `health` | Check server health and component status |
| `benchmark` | Load-test a channel and report latency statistics |
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
| `packages` | List and inspect package promotion receipts |
| `dlq` | Inspect and requeue the trace dead-letter queue |
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

## Workflow Management

Full lifecycle management for [Orion workflows](https://docs.goplasmatic.io/reference/admin-api.html):

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
orion-cli workflows test <ID> -d '{"order_id":"ORD-9182","total":25000}' --trace
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
orion-cli channels create -d '{"name":"orders","channel_type":"sync","protocol":"rest","route_pattern":"/orders/{id}","methods":["GET"],"workflow_id":"process-orders"}'

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

Manage [named external service configurations](https://docs.goplasmatic.io/reference/connectors.html) with auth and retry policies:

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

[Processing modes](https://docs.goplasmatic.io/reference/data-api.html) for any workload:

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

## AI Assistants

Install the [Orion agent skill](../../skills/orion/) and an AI coding agent can
drive every command below — authoring workflow JSON, dry-running it, and walking
the draft → test → activate path on its own.

```bash
mkdir -p .claude/skills && cp -r skills/orion .claude/skills/
```

The skill is knowledge, not a service: the agent acts through this CLI under
your shell, so it inherits exactly your access, every admin write lands in the
audit log under your principal, and nothing new listens on a port.

[Agent Skill Setup](https://docs.goplasmatic.io/ai/skills.html) covers the
machine-wide install, what the skill knows, and how to give an agent its own
scoped credentials. For an assistant with no shell, the
[prompt pack](https://docs.goplasmatic.io/ai/prompt-pack.html) drives the plain
REST API instead.

> **Removed in 1.2.0:** `orion-cli mcp serve`. Its HTTP transport put the full
> admin API on a port with no authentication of its own, and every one of its
> tools mirrored a command this CLI already had. The agent skill covers the same
> ground with a smaller attack surface.

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
orion-cli workflows test "$WF_ID" -d '{"amount":100}'
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
# Docker
docker run --rm ghcr.io/goplasmatic/orion-cli:latest --server http://host.docker.internal:8080 health

# macOS (Homebrew)
brew install GoPlasmatic/tap/orion-cli

# macOS / Linux / Windows installers: shell and PowerShell one-liners are
# attached to each orion-cli-v* release:
# https://github.com/GoPlasmatic/Orion/releases

# From source
cargo install --git https://github.com/GoPlasmatic/Orion orion-cli
```

Verify with `orion-cli --version`. Requires Rust 1.88+ for source builds.

---

## Related

- **[Orion Server](https://github.com/GoPlasmatic/Orion)** — The services runtime platform
- **[API Reference](https://docs.goplasmatic.io/reference/admin-api.html)** — Full REST API documentation
- **[Connectors Guide](https://docs.goplasmatic.io/reference/connectors.html)** — Auth schemes, retry policies, and secrets
- **[Production Features](https://docs.goplasmatic.io/reference/cli.html)** — Custom IDs, versioning, fault tolerance
- **[Use Cases & Patterns](https://docs.goplasmatic.io/guides/worked-examples.html)** — Real-world examples and AI prompt templates
- **[Observability](https://docs.goplasmatic.io/operate/monitoring.html)** — Prometheus metrics, health checks, logging

## Contributing

Contributions are welcome! Please open an issue or submit a pull request on [GitHub](https://github.com/GoPlasmatic/Orion).

```bash
cargo build          # Build
cargo test           # Run tests
cargo clippy         # Lint
cargo fmt            # Format
```

## License

Apache-2.0 — see [LICENSE](LICENSE) for details.

---

If Orion CLI is useful to you, a ⭐ on [GitHub](https://github.com/GoPlasmatic/Orion) helps other developers find it.
