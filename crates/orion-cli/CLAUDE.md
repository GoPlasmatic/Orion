# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working on the orion-cli crate.

## Project Overview

Orion CLI is a Rust CLI and MCP server for the Orion services runtime — the `crates/orion-server` crate in this same workspace (the repo root `CLAUDE.md` covers the workspace layout; bare cargo commands there default to the server, so use `-p orion-cli` from the root or run cargo inside this crate directory). It manages workflows, channels, connectors, data processing, engine health, and traces via HTTP against an Orion server. The binary also includes a built-in MCP server (`orion-cli mcp serve`) for AI tool integration (Claude Desktop, Cursor, etc.).

## Build & Development Commands

```bash
cargo build -p orion-cli       # Build (from the workspace root)
cargo test -p orion-cli        # Run unit tests
cargo fmt --all --check        # Check formatting
cargo clippy -p orion-cli --all-targets -- -D warnings  # Lint (CI treats warnings as errors)
cargo deny check               # Dependency/licence policy (whole workspace)
```

### E2E Tests

The end-to-end suite lives at the **repo root** (`tests/e2e/`), not in this
crate — it exercises the server and the CLI together.

```bash
# From the workspace root — builds both binaries from this tree (needs jq, curl):
just e2e

# Or directly; ORION_BIN/ORION_CLI override the in-tree default binaries:
./tests/e2e/run.sh
ORION_BIN=/path/to/orion-server ./tests/e2e/run.sh

# Useful env vars:
# E2E_PORT=9090        Custom port
# E2E_DEBUG=1          Debug logging
# E2E_SKIP_BUILD=1     Skip cargo build, use existing binaries
# E2E_KEEP_SERVER=1    Don't stop server after tests
```

### MCP Server

```bash
orion-cli mcp serve                           # stdio transport (Claude Desktop / Cursor)
orion-cli mcp serve --http                    # HTTP transport (remote clients), default bind 0.0.0.0:8081
orion-cli mcp serve --http --bind 0.0.0.0:9090  # HTTP on custom address
```

E2E tests are shell-based (not `cargo test`). 13 test suites in `tests/e2e/suites/` and fixtures in `tests/e2e/fixtures/` (both at the repo root); suite 13 is data-driven — scenario cases in `examples/use-cases/` (referencing the example packages' workflows by file) plus runtime-behaviour cases in `tests/e2e/cases/` — and suite 14 smoke-covers the read-only command groups the lifecycle suites never reach. The suites speak the v1.0 API: every send goes through a channel bound to exactly one workflow (`create_channel` in helpers.sh), and reading a trace needs the `trace_token` from the async submit.

## Architecture

**Rust 1.88+ (workspace MSRV), edition 2024, async with Tokio.**

### Module Layout

- `src/main.rs` — Entry point, clap CLI definition with global flags (`--server`, `--output`, `--quiet`, `--verbose`, `--no-color`, `--yes`)
- `src/client.rs` — thin presentation adapter over the shared `orion-client` crate's `OrionClient` (the transport itself — auth, 30s timeout, envelope parsing, typed `ClientError` — lives there); this wrapper translates typed errors into the CLI's terminal messages (hints, `[CODE] message`, field errors). Endpoint paths come from `orion_client::paths`, never format strings.
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
| `packages.rs` | Package promotion receipts: list, get (v1.0) |
| `dlq.rs` | Trace dead-letter queue: list, get, requeue, purge (v1.0) |
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

Core: `clap` (derive) for CLI, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output, and the two workspace library crates: `orion-api` for the shared wire contract — the error envelope (`ErrorEnvelope`, `codes`), status vocabulary (`STATUS_ACTIVE`…), and the typed `ImportResult` are the same definitions the server serializes — and `orion-client` for the HTTP transport (`OrionClient`, `paths`, `query_string`; it owns reqwest — the CLI has no direct HTTP dependency). MCP: `rmcp` (server, transport-io, transport-streamable-http-server), `schemars`, `tracing`/`tracing-subscriber`, `axum`.

### CI/CD

Workflows live at the repo root (`.github/workflows/`), shared with the server:

- **CI** (`ci.yml`): fmt/clippy/test run `--workspace`; the `cli-e2e` job builds both binaries and runs the e2e suite; `deny` covers the unified lockfile
- **Release** (`release.yml`): an `orion-cli-vX.Y.Z` tag drives cargo-dist v0.31.0 cross-platform builds (macOS ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Docker + MCP registry** (`docker-release-cli.yml`): same tag builds the ghcr.io/goplasmatic/orion-cli image and republishes `server.json` to the MCP registry
- **Homebrew tap:** GoPlasmatic/homebrew-tap
