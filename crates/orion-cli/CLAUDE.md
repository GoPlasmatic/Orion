# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working on the orion-cli crate.

## Project Overview

Orion CLI is a Rust CLI for the Orion services runtime — the `crates/orion-server` crate in this same workspace (the repo root `CLAUDE.md` covers the workspace layout; bare cargo commands there default to the server, so use `-p orion-cli` from the root or run cargo inside this crate directory). It deploys compiled definition sets as v2 deployments, sends data, and reads engine status, health, traces, schedules and the operational ledgers via HTTP against an Orion server. Definitions reach a node only through a deployment; the v1 entity commands (`workflows`, `channels`, `connectors`, `plugins`, `packages`) and `benchmark` are gone.

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

E2E tests are shell-based (not `cargo test`). 11 test suites in `tests/e2e/suites/` and fixtures in `tests/e2e/fixtures/` (both at the repo root). Every suite deploys what it needs the way an operator does: `new_set` / `set_workflow` / `set_channel` / `set_plugin` write a definition set, and `deploy_set` compiles it with `orion-server compile` and sends it with `deployments apply -d` (helpers.sh). An apply replaces the head, so a suite needing several services deploys them as one set. Suite 06 is data-driven: scenario cases in `examples/use-cases/` (each deploys an example package) plus runtime-behaviour cases in `tests/e2e/cases/`. Suite 07 smoke-covers the read-only command groups, and suite 11 covers deployments themselves (plan, removal, rollback). `tests/deployments_dir_e2e.rs` is the compile → apply flow as a `cargo test`, against the `orion-server` that `cargo test --workspace` builds beside this crate's binary. Reading a trace needs the `trace_token` from the async submit.

## Architecture

**Rust 1.98+ (workspace MSRV), edition 2024, async with Tokio.**

### Module Layout

- `src/main.rs` — Entry point, clap CLI definition with global flags (`--server`, `--api-key`, `--api-key-header`, `--change-context`, `--output`, `--quiet`, `--verbose`, `--no-color`, `--yes`). `build_client()` resolves the API key flag → `ORION_API_KEY` → `~/.orion/config.toml` via `OrionConfig::resolve_api_key`, and applies `--change-context` as the `X-Orion-Change-Context` header on every request.
- `src/client.rs` — thin presentation adapter over the shared `orion-client` crate's `OrionClient` (the transport itself — auth, 30s timeout, envelope parsing, typed `ClientError` — lives there); this wrapper translates typed errors into the CLI's terminal messages (hints, `[CODE] message`, field errors). Endpoint paths come from `orion_client::paths`, never format strings.
- `src/config.rs` — `OrionConfig` loaded from `~/.orion/config.toml` (server_url, default_output), plus `resolve_api_key()` for the flag → env → file chain
- `src/output.rs` — Output formatting: `print_table()` (tabled with rounded borders), `print_value()` (JSON/YAML)
- `src/utils.rs` — shared command helpers: `wait_for_trace()` (shared by `traces wait` and `send --wait`), `read_json_input()`, `confirm()`, the status and duration formatters, and `print_list_footer()`, the renderer every list goes through (reads the envelope's top-level `total`; says `Showing N of M` when the page is short of it)
- `src/commands/` — One file per command group, each defining clap subcommands and `execute()` async functions

### Command Modules

| Module | Key functionality |
|---|---|
| `data.rs` | Send data: sync (`--profile` renders `_orion.profile`), async (wait/timeout/trace tracking; handles null trace_id when tracing is off), `--raw` for `body_mode = "payload"` channels |
| `traces.rs` | Execution trace viewing and polling (shows `task_trace_json` when present); `--token` carries the per-submission capability token |
| `engine.rs` | Engine status: what this node serves, and its load issues |
| `functions.rs` | List registered workflow task functions and their input schemas |
| `health.rs` | Health check with component status, exit code 1 if degraded |
| `metrics.rs` | Raw Prometheus metrics retrieval |
| `audit_logs.rs` | List audit log entries, with the endpoint's full filter set (`--action`, `--resource-type`, `--resource-id`, `--principal`, `--start-time`, `--end-time`) |
| `backups.rs` | Create and list database backups (SQLite) |
| `cache.rs` | `invalidate <namespace>`: bump a channel response-cache namespace |
| `cron.rs` | The scheduled-run ledger: `status` (per-channel cursor, next fire, last result), `list` (filterable by `--channel-id`/`--status`), `get`, `retry` — another attempt at the same occurrence, keeping its id and `scheduled_for` — `cancel`, which settles a pending or running occurrence failed and frees its singleton slot, and `trigger <channel>`, a new occurrence *now* (`POST /api/v2/admin/channels/{id}/trigger`) |
| `dlq.rs` | Trace dead-letter queue: list, get, requeue, purge (v1.0) |
| `v2.rs` + `deployment_dir.rs` | The v2 surfaces over `orion_client::V2Admin`'s typed calls: each first reads the server's `api_version` (`require_api_version`, called from `main.rs`) and refuses one this CLI does not speak; `deployments` (capabilities/plan/apply/list/get/wait/rollback/edition-preflight), `provider` (list/get/upload/admit/revoke/scaffold/lint/pack), `connections` (list/get/create/probe — `probe` exits 2 when the backend is unhealthy). `deployments plan\|apply -d <dir>` deploys a directory `orion-server compile` wrote: `deployment_dir.rs` uploads its definitions (each answered digest must be its file name), reuses or creates connection revisions, assigns member revisions against the target's history (kept when unchanged, taken back when an earlier revision had the same definition, else one past the highest), plans against the head and applies with the reviewed digest; an unchanged directory applies nothing |
| `config.rs` | CLI config management (set-server, show, set key-value) |
| `completions.rs` | Shell completion generation (bash/zsh/fish/powershell/elvish) |

Shared logic lives in `src/utils.rs`. A command module holds its clap definitions, its own tables and detail views, and the verbs unique to it.

**Every list surface pages** (50 default, 1000 max, server-clamped), and traces also sort. Every list command must carry `--limit`/`--offset`/`--sort-by`/`--sort-order` — one that omits them silently truncates at 50 with no way to see the rest.

### Key Patterns

- **Config precedence:** CLI flags > env vars (`ORION_SERVER_URL`, `ORION_API_KEY`, `ORION_API_KEY_HEADER`, `ORION_CHANGE_CONTEXT`, `NO_COLOR`) > `~/.orion/config.toml`. The env vars are declared on the clap args themselves, so `cli.server` already carries `ORION_SERVER_URL` — there is no second resolver for it.
- **Exit codes carry the answer.** `health` exits 1 when the server reports itself degraded, even though the HTTP status is **200**. A command must read the answer, not only the HTTP status, or a failing check looks like a passing one.
- **Output formats:** table (default), json, yaml — controlled by `--output` flag
- **Error handling:** `anyhow` throughout; `OrionClient` parses server error responses with codes/messages and renders the v0.2 structured `error.details[]` (field-pathed validation errors) and `request_id` when present
- **All commands are async** — Tokio runtime, reqwest for HTTP

### Dependencies

Core: `clap` (derive) for CLI, `tokio` for async, `serde`/`serde_json`/`serde_yaml`/`toml` for serialization, `anyhow` for errors, `colored` + `tabled` for terminal output, and the two workspace library crates: `orion-api` for the shared wire contract — the error envelope (`ErrorEnvelope`, `codes`) and the status vocabulary (`STATUS_ACTIVE`…) are the same definitions the server serializes — and `orion-client` for the HTTP transport (`OrionClient`, `paths`, `query_string`; it owns reqwest — the CLI has no direct HTTP dependency).

### CI/CD

Workflows live at the repo root (`.github/workflows/`), shared with the server:

- **CI** (`ci.yml`): fmt/clippy/test run `--workspace`; the `cli-e2e` job builds both binaries and runs the e2e suite; `deny` covers the unified lockfile
- **Release** (`release.yml`): an `orion-cli-vX.Y.Z` tag drives cargo-dist v0.31.0 cross-platform builds (macOS ARM, Linux x86_64/ARM, Windows) with shell/powershell/homebrew installers
- **Docker** (`docker-release-cli.yml`): same tag builds the ghcr.io/goplasmatic/orion-cli image
- **Homebrew tap:** GoPlasmatic/homebrew-tap
