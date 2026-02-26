# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion CLI is a Rust CLI for the [Orion rules engine platform](https://github.com/GoPlasmatic/Orion). It manages rules, connectors, data channels, engine health, and async jobs via HTTP against an Orion server.

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

E2E tests are shell-based (not `cargo test`). 13 test suites in `tests/e2e/suites/`, fixtures in `tests/e2e/fixtures/`, test case definitions in `tests/e2e/cases/`.

## Architecture

**Rust 1.85+, edition 2024, async with Tokio.**

### Module Layout

- `src/main.rs` — Entry point, clap CLI definition with global flags (`--server`, `--output`, `--quiet`, `--verbose`, `--no-color`, `--yes`)
- `src/client.rs` — `OrionClient` HTTP wrapper around reqwest (30s timeout, JSON request/response, structured error handling)
- `src/config.rs` — `CliConfig` loaded from `~/.orion/config.toml` (server_url, default_output)
- `src/output.rs` — Output formatting: `print_table()` (tabled with rounded borders), `print_value()` (JSON/YAML)
- `src/commands/` — One file per command group, each defining clap subcommands and `execute()` async functions

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

### Key Patterns

- **Config precedence:** CLI flags > env vars (`ORION_SERVER_URL`, `NO_COLOR`) > `~/.orion/config.toml`
- **Output formats:** table (default), json, yaml — controlled by `--output` flag
- **Error handling:** `anyhow` throughout; `OrionClient` parses server error responses with codes and messages
- **All commands are async** — Tokio runtime, reqwest for HTTP

### Dependencies

Core: `clap` (derive) for CLI, `reqwest` (rustls-tls) for HTTP, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output.

### CI/CD

- **CI** (`ci.yml`): fmt check → clippy → test → audit on ubuntu-22.04
- **Release** (`release.yml`): Triggered by version tags, uses cargo-dist v0.31.0 for cross-platform builds (macOS Intel/ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Homebrew tap:** GoPlasmatic/homebrew-tap
