# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working on the orion-cli crate.

## Project Overview

Orion CLI is a Rust CLI for the Orion services runtime — the `crates/orion-server` crate in this same workspace (the repo root `CLAUDE.md` covers the workspace layout; bare cargo commands there default to the server, so use `-p orion-cli` from the root or run cargo inside this crate directory). It manages workflows, channels, connectors, data processing, engine health, and traces via HTTP against an Orion server.

AI assistants drive this CLI through the **agent skill** at `skills/orion/` (repo root), not through a tool server. `orion-cli mcp serve` and the whole `src/mcp/` tree were removed in 1.2.0: the HTTP transport exposed the full admin API on a port with no authentication of its own, and all 58 tools were hand-written mirrors of `commands::`. Do not reintroduce one — extend the skill instead, and keep anything discoverable at runtime (`functions list`, `--help`) out of it.

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

E2E tests are shell-based (not `cargo test`). 13 test suites in `tests/e2e/suites/` and fixtures in `tests/e2e/fixtures/` (both at the repo root); suite 13 is data-driven — scenario cases in `examples/use-cases/` (referencing the example packages' workflows by file) plus runtime-behaviour cases in `tests/e2e/cases/` — and suite 14 smoke-covers the read-only command groups the lifecycle suites never reach. The suites speak the v1.0 API: every send goes through a channel bound to exactly one workflow (`create_channel` in helpers.sh), and reading a trace needs the `trace_token` from the async submit.

## Architecture

**Rust 1.98+ (workspace MSRV), edition 2024, async with Tokio.**

### Module Layout

- `src/main.rs` — Entry point, clap CLI definition with global flags (`--server`, `--api-key`, `--api-key-header`, `--change-context`, `--output`, `--quiet`, `--verbose`, `--no-color`, `--yes`). `build_client()` resolves the API key flag → `ORION_API_KEY` → `~/.orion/config.toml` via `OrionConfig::resolve_api_key`, and applies `--change-context` as the `X-Orion-Change-Context` header on every request.
- `src/client.rs` — thin presentation adapter over the shared `orion-client` crate's `OrionClient` (the transport itself — auth, 30s timeout, envelope parsing, typed `ClientError` — lives there); this wrapper translates typed errors into the CLI's terminal messages (hints, `[CODE] message`, field errors). Endpoint paths come from `orion_client::paths`, never format strings.
- `src/config.rs` — `OrionConfig` loaded from `~/.orion/config.toml` (server_url, default_output), plus `resolve_api_key()` for the flag → env → file chain
- `src/output.rs` — Output formatting: `print_table()` (tabled with rounded borders), `print_value()` (JSON/YAML)
- `src/utils.rs` — shared command helpers. **The CRUD commands live here, not in the command modules**: `create_entity`/`update_entity`/`delete_entity`/`validate_entity`/`export_entities` take an `EntityKind` (the entity's nouns and endpoints, declared once as a `static` in its own command module), and `change_status`/`list_versions`/`create_version` take a `VersionedEntityKind` — which connectors deliberately do not have, so an unversioned entity cannot reach them. Also `run_import()`, `wait_for_trace()` (shared by `traces wait` and `send --wait`), `read_json_input()`, `confirm()`, and the two renderers every list and validation path goes through — `print_list_footer()` (reads the envelope's top-level `total`; says `Showing N of M` when the page is short of it) and `print_validation_envelope()` (the `{valid, errors, warnings}` shape, returning exit 1 when invalid)
- `src/commands/` — One file per command group, each defining clap subcommands and `execute()` async functions

### Command Modules

| Module | Key functionality |
|---|---|
| `workflows.rs` (largest) | Full CRUD, status transitions (activate/archive with `--dry-run`/`--defer-reload`), test dry-run, rollout (`--defer-reload`), versioning, import (server-side `?dry_run`, `--on-conflict`)/export with diff |
| `channels.rs` | Channel CRUD, status transitions (same two flags), versioning, validate, filtered export, bulk import |
| `data.rs` | Send data: sync (`--profile` renders `_orion.profile`), async (wait/timeout/trace tracking; handles null trace_id when tracing is off), `--raw` for `body_mode = "payload"` channels |
| `connectors.rs` | Connector CRUD, enable/disable, test probe, circuit breaker management, bulk import |
| `traces.rs` | Execution trace viewing and polling (shows `task_trace_json` when present); `--token` carries the per-submission capability token |
| `engine.rs` | Engine status, hot-reload |
| `functions.rs` | List registered workflow task functions and their input schemas |
| `health.rs` | Health check with component status, exit code 1 if degraded |
| `metrics.rs` | Raw Prometheus metrics retrieval |
| `audit_logs.rs` | List audit log entries, with the endpoint's full filter set (`--action`, `--resource-type`, `--resource-id`, `--principal`, `--start-time`, `--end-time`) |
| `backups.rs` | Create and list database backups (SQLite) |
| `packages.rs` | Package promotion receipts: list, get (v1.0) |
| `dlq.rs` | Trace dead-letter queue: list, get, requeue, purge (v1.0) |
| `config.rs` | CLI config management (set-server, show, set key-value) |
| `completions.rs` | Shell completion generation (bash/zsh/fish/powershell/elvish) |

Shared logic lives in `src/utils.rs`: the entity CRUD commands (driven by each module's `EntityKind` static), bulk import (`run_import()`), and the trace wait. A command module holds its clap definitions, its own tables and detail views, and the verbs unique to it — not a fourth copy of `create`.

**Every list surface pages** (50 default, 1000 max, server-clamped) and the versioned lists plus connectors and traces also sort. Every list command must carry `--limit`/`--offset`/`--sort-by`/`--sort-order` — one that omits them silently truncates at 50 with no way to see the rest.

### Key Patterns

- **Config precedence:** CLI flags > env vars (`ORION_SERVER_URL`, `ORION_API_KEY`, `ORION_API_KEY_HEADER`, `ORION_CHANGE_CONTEXT`, `NO_COLOR`) > `~/.orion/config.toml`. The env vars are declared on the clap args themselves, so `cli.server` already carries `ORION_SERVER_URL` — there is no second resolver for it.
- **Exit codes carry the answer.** `validate` and `activate/archive --dry-run` both return the `{valid, errors, warnings}` envelope inside a **200** — a refused transition is a finding, not an HTTP failure — so the command must read `valid` and exit 1. Reading only the HTTP status makes a failing pre-flight look like a passing one.
- **Output formats:** table (default), json, yaml — controlled by `--output` flag
- **Error handling:** `anyhow` throughout; `OrionClient` parses server error responses with codes/messages and renders the v0.2 structured `error.details[]` (field-pathed validation errors) and `request_id` when present
- **All commands are async** — Tokio runtime, reqwest for HTTP

### Dependencies

Core: `clap` (derive) for CLI, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output, `tokio-util` for the benchmark runner's cancellation, and the two workspace library crates: `orion-api` for the shared wire contract — the error envelope (`ErrorEnvelope`, `codes`), status vocabulary (`STATUS_ACTIVE`…), and the typed `ImportResult` are the same definitions the server serializes — and `orion-client` for the HTTP transport (`OrionClient`, `paths`, `query_string`; it owns reqwest — the CLI has no direct HTTP dependency).

### CI/CD

Workflows live at the repo root (`.github/workflows/`), shared with the server:

- **CI** (`ci.yml`): fmt/clippy/test run `--workspace`; the `cli-e2e` job builds both binaries and runs the e2e suite; `deny` covers the unified lockfile
- **Release** (`release.yml`): an `orion-cli-vX.Y.Z` tag drives cargo-dist v0.31.0 cross-platform builds (macOS ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Docker** (`docker-release-cli.yml`): same tag builds the ghcr.io/goplasmatic/orion-cli image
- **Homebrew tap:** GoPlasmatic/homebrew-tap
