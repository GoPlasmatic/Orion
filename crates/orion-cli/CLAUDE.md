# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion CLI is a Rust CLI and MCP server for the [Orion services runtime](https://github.com/GoPlasmatic/Orion). It manages workflows, channels, connectors, data processing, engine health, and traces via HTTP against an Orion server. The binary also includes a built-in MCP server (`orion-cli mcp serve`) for AI tool integration (Claude Desktop, Cursor, etc.).

## Build & Development Commands

```bash
cargo build                    # Build
cargo build --release          # Release build
cargo test                     # Run unit tests
cargo fmt --check              # Check formatting
cargo clippy --all-targets -- -D warnings  # Lint (CI treats warnings as errors)
cargo audit                    # Security audit
```

### E2E Tests

```bash
# Requires: orion-server binary, jq, curl
ORION_BIN=/path/to/orion-server ./tests/e2e/run.sh

# Useful env vars:
# E2E_PORT=9090        Custom port
# E2E_DEBUG=1          Debug logging
# E2E_SKIP_BUILD=1     Skip cargo build, use existing binary
# E2E_KEEP_SERVER=1    Don't stop server after tests
```

### MCP Server

```bash
orion-cli mcp serve                           # stdio transport (Claude Desktop / Cursor)
orion-cli mcp serve --http                    # HTTP transport (remote clients), default bind 0.0.0.0:8081
orion-cli mcp serve --http --bind 0.0.0.0:9090  # HTTP on custom address
```

E2E tests are shell-based (not `cargo test`). 13 test suites in `tests/e2e/suites/`, fixtures in `tests/e2e/fixtures/`, test case definitions in `tests/e2e/cases/`.

## Architecture

**Rust 1.85+, edition 2024, async with Tokio.**

### Module Layout

- `src/main.rs` — Entry point, clap CLI definition with global flags (`--server`, `--output`, `--quiet`, `--verbose`, `--no-color`, `--yes`)
- `src/client.rs` — `OrionClient` HTTP wrapper around reqwest (30s timeout, JSON request/response, structured error handling)
- `src/config.rs` — `OrionConfig` loaded from `~/.orion/config.toml` (server_url, default_output), includes `resolve_server_url()` for MCP
- `src/output.rs` — Output formatting: `print_table()` (tabled with rounded borders), `print_value()` (JSON/YAML)
- `src/commands/` — One file per command group, each defining clap subcommands and `execute()` async functions
- `src/mcp/` — MCP server module (OrionService with tool_router/tool_handler, serve function for stdio/HTTP)
- `src/mcp/tools/` — MCP tool implementations (workflows, channels, connectors, circuit_breakers, data, traces, engine, functions, health, metrics, audit_logs, backups). `tools/mod.rs` also holds the shared `import_resource()` bulk-import helper.
- `src/mcp/tools/descriptions/` — Markdown files with detailed tool descriptions for MCP clients

### Command Modules

| Module | Key functionality |
|---|---|
| `workflows.rs` (largest) | Full CRUD, status transitions (activate/archive), test dry-run, rollout, versioning, import (server-side `?dry_run`)/export with diff |
| `channels.rs` | Channel CRUD, status transitions, versioning, bulk import |
| `data.rs` | Send data: sync (`--profile` renders `_orion.profile`), async (wait/timeout/trace tracking; handles null trace_id when tracing is off) |
| `connectors.rs` | Connector CRUD, enable/disable, circuit breaker management, bulk import |
| `traces.rs` | Execution trace viewing and polling (shows `task_trace_json` when present) |
| `engine.rs` | Engine status, hot-reload |
| `functions.rs` | List registered workflow task functions and their input schemas |
| `health.rs` | Health check with component status, exit code 1 if degraded |
| `metrics.rs` | Raw Prometheus metrics retrieval |
| `audit_logs.rs` | List audit log entries of admin actions |
| `backups.rs` | Create and list database backups (SQLite) |
| `config.rs` | CLI config management (set-server, show, set key-value) |
| `completions.rs` | Shell completion generation (bash/zsh/fish/powershell) |
| `mcp.rs` | MCP server subcommand (`orion-cli mcp serve`) |

Shared bulk-import logic lives in `utils::run_import()` (CLI) and `mcp::tools::import_resource()` (MCP).

### Key Patterns

- **Config precedence:** CLI flags > env vars (`ORION_SERVER_URL`, `NO_COLOR`) > `~/.orion/config.toml`
- **Output formats:** table (default), json, yaml — controlled by `--output` flag
- **Error handling:** `anyhow` throughout; `OrionClient` parses server error responses with codes/messages and renders the v0.2 structured `error.details[]` (field-pathed validation errors) and `request_id` when present
- **All commands are async** — Tokio runtime, reqwest for HTTP

### Dependencies

Core: `clap` (derive) for CLI, `reqwest` (rustls-tls) for HTTP, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output. MCP: `rmcp` (server, transport-io, transport-streamable-http-server), `schemars`, `tracing`/`tracing-subscriber`, `axum`.

### CI/CD

- **CI** (`ci.yml`): fmt check → clippy → test → audit on ubuntu-22.04
- **Release** (`release.yml`): Triggered by version tags, uses cargo-dist v0.31.0 for cross-platform builds (macOS Intel/ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Homebrew tap:** GoPlasmatic/homebrew-tap
