# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion CLI is a Rust CLI and MCP server for the [Orion rules engine platform](https://github.com/GoPlasmatic/Orion). It manages rules, connectors, data channels, engine health, and async jobs via HTTP against an Orion server. The binary also includes a built-in MCP server (`orion mcp serve`) for AI tool integration (Claude Desktop, Cursor, etc.).

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
orion mcp serve                           # stdio transport (Claude Desktop / Cursor)
orion mcp serve --http                    # HTTP transport (remote clients), default bind 0.0.0.0:8081
orion mcp serve --http --bind 0.0.0.0:9090  # HTTP on custom address
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
- `src/mcp/tools/` — MCP tool implementations (rules, connectors, data, traces, engine, health, metrics)
- `src/mcp/tools/descriptions/` — Markdown files with detailed tool descriptions for MCP clients

### Command Modules

| Module | Key functionality |
|---|---|
| `rules.rs` (largest, ~716 lines) | Full CRUD, status transitions (activate/pause/archive), test dry-run, import/export with diff |
| `data.rs` | Send data: sync, async (with wait/timeout/job tracking), batch modes |
| `connectors.rs` | Connector CRUD, enable/disable |
| `jobs.rs` | Async job status polling with configurable interval/timeout |
| `engine.rs` | Engine status, hot-reload |
| `health.rs` | Health check with component status, exit code 1 if degraded |
| `metrics.rs` | Raw Prometheus metrics retrieval |
| `config.rs` | CLI config management (set-server, show, set key-value) |
| `completions.rs` | Shell completion generation (bash/zsh/fish/powershell) |
| `mcp.rs` | MCP server subcommand (`orion mcp serve`) |

### Key Patterns

- **Config precedence:** CLI flags > env vars (`ORION_SERVER_URL`, `NO_COLOR`) > `~/.orion/config.toml`
- **Output formats:** table (default), json, yaml — controlled by `--output` flag
- **Error handling:** `anyhow` throughout; `OrionClient` parses server error responses with codes and messages
- **All commands are async** — Tokio runtime, reqwest for HTTP

### Dependencies

Core: `clap` (derive) for CLI, `reqwest` (rustls-tls) for HTTP, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output. MCP: `rmcp` (server, transport-io, transport-streamable-http-server), `schemars`, `tracing`/`tracing-subscriber`, `axum`.

### CI/CD

- **CI** (`ci.yml`): fmt check → clippy → test → audit on ubuntu-22.04
- **Release** (`release.yml`): Triggered by version tags, uses cargo-dist v0.31.0 for cross-platform builds (macOS Intel/ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Homebrew tap:** GoPlasmatic/homebrew-tap
