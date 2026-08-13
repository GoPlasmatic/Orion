# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a declarative services runtime written in Rust. It exposes business logic management through channels (service endpoints) and workflows (task pipelines powered by dataflow-rs) via a REST API. Ships as a single binary with an embedded SQLite database.

This repo is a cargo workspace (monorepo) with four crates: `crates/orion-server` (the runtime — everything below describes it), `crates/orion-cli` (the CLI + MCP server, which has its own `CLAUDE.md`), and two shared library crates. `crates/orion-api` is the wire contract: response DTOs, domain enums, the error envelope + `codes` registry, and the bulk-import report — the server serializes these types, clients deserialize them; the server re-exports them under their pre-1.0 paths (e.g. `crate::errors::FieldError`, `storage::models::EntityStatus`). Its deserialization is tolerant by design (every field defaults, unknown fields ignored) so version skew between server and CLI keeps working; its `utoipa` feature (enabled only by the server) adds the `ToSchema` derives for the OpenAPI document. `crates/orion-client` is the one HTTP transport over that contract — `OrionClient` (auth, envelope unwrap, typed `ClientError`) plus the `paths` module every endpoint path is built from; both `orion-cli` and the server's `package_cli` drive it, and it is the only crate that owns reqwest for API calls. Neither library crate has a release cycle of its own — no tag names them; `crates-publish.yml` publishes them automatically as riders (skip-if-present, dependency order) before a binary crate, because crates.io refuses a crate whose dependency it doesn't host. The one rule this imposes: **bump a rider crate's version with any change to it**, or the rider skips and the published binary resolves the older crates.io content. `default-members` points bare cargo commands at the server; use `--workspace` or `-p orion-cli` to reach the CLI. Only the UI lives in a separate repo (Orion-ui).

- **Rust Edition:** 2024. **MSRV: 1.88** (`rust-version` in Cargo.toml) — driven by let-chains (`if let Some(x) = a && let Some(y) = b`, stabilized in 1.88) and dependency requirements (`mongodb`, `serde_with`, `time`, `tonic`).
- **Core dependencies:** `dataflow-rs` 3.3 (workflow engine), `axum` 0.8 (HTTP), `sqlx` 0.8 (database), `sea-query` 0.32 (portable SQL builder)
- **`datalogic-rs` 5 (JSONLogic) and `datavalue` are reached through `dataflow-rs`**, not pinned directly. dataflow-rs's public API is written in terms of both — `TaskContext::datalogic()` returns `&Arc<datalogic_rs::Engine>`, the whole context/path surface is `datavalue::OwnedDataValue` — so a second pin would let their major versions skew from the ones dataflow-rs links. Add `use dataflow_rs::datalogic_rs;` (or `::datavalue`) to a module that needs them; a bare `datalogic_rs::` path will not resolve, and note that a file-level `use` does **not** reach an inner `#[cfg(test)] mod tests`. Orion cannot enable a datalogic feature directly, but dataflow-rs 3.2 added `all-operators`, which passes through to the `datetime`, `ext-string`, `ext-array`, `ext-math`, `ext-control` and `error-handling` gates — and the server enables it, so those operators *are* available. `docs/src/reference/expressions.md#available-operators` is the resulting vocabulary, asserted against the engine by `jsonlogic_operators_test.rs`.
- **Binaries:** `orion-server` (`crates/orion-server/src/main.rs`) and `orion-cli` (`crates/orion-cli/src/main.rs`)

## Build & Development Commands

```bash
cargo build                        # Build (all features included)
cargo build --release              # Release build

cargo run -- --config ./config.toml  # Run with config file

cargo test                         # Run the server suite (default-members)
cargo test --workspace             # Server + CLI suites — what CI runs
cargo test <test_name>             # Run a single test by name

cargo clippy --workspace --all-targets  # Lint (matches CI)
cargo fmt --all                    # Format code

just e2e                           # End-to-end suite (tests/e2e): CLI against a real orion-server
```

Docker: `docker build -t orion .` for the server; `docker build -f crates/orion-cli/Dockerfile -t orion-cli .` for the CLI (both multi-stage from the workspace-root context).

## Runtime Configuration

All capabilities are compiled into a single binary — no feature flags. Behaviour is controlled at runtime:

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme (`sqlite:`, `postgres://`, `mysql://`) | SQLite |
| Kafka | `kafka.enabled` | Disabled |
| OpenTelemetry | `tracing.enabled` | Disabled |
| Trace persistence mode | `trace_storage.mode` (`sync` / `async` / `batch` / `off`) — global default with per-channel override via `config.tracing` | Sync |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI / OpenAPI spec | `server.docs.enabled` (unset = enabled outside production) | Enabled outside production |
| SQL connectors | `db_read`/`db_write` functions | Always available |
| Redis cache | `cache_read`/`cache_write` with Redis backend | Always available |
| MongoDB connector | `mongo_read` function | Always available |

## Architecture

### Module Structure

Paths below are relative to `crates/orion-server/`.

```
src/
├── main.rs              # clap CLI entrypoint; declares the binary-only cli/package_cli modules
├── bootstrap.rs         # Startup sequence: config → pools → repos → engine → HTTP server
├── cli.rs               # Diagnostic subcommands: validate-config, migrate, lint, dry-run, test, test-connectivity, dump-openapi
├── preflight.rs         # `orion-server preflight` — scans the stored estate for upgrade breaks
├── package_cli.rs       # `orion-server package` — export/lint/plan/apply/diff promotion CLI
├── lib.rs               # Public module declarations
├── channel/             # Channel registry, config, routing, rate limiting, request guards
├── cluster/             # Multi-node coordination: epoch watcher, job leases
├── config/              # Configuration loading & validation
├── connector/           # Connector types, registry, circuit breakers, pool caching, secret resolution
├── engine/              # Dataflow engine build/reload, observer, custom function handlers
│   └── functions/       # http_call, channel_call, db_read/write, data_query/write, cache_read/write, mongo_read, publish_kafka
├── errors.rs            # OrionError enum → HTTP response mapping
├── kafka/               # Kafka producer & consumer
├── metrics.rs           # Prometheus metrics collection
├── query/               # Portable data dialect: IR, lowering, schema; backends sql/mongo/es
├── queue/               # Async trace/audit processing, DLQ retry
├── server/              # HTTP server, middleware, state
│   └── routes/          # admin/ (workflows, channels, connectors, packages, functions, engine, audit, backups, trace_dlq), data/
├── storage/             # Database abstraction, content hashing, config encryption
│   ├── models/          # Row types, DTOs, enums
│   └── repositories/    # workflows, channels, connectors, packages, traces, trace_dlq, audit_logs, cluster
└── validation/          # Input validation, SSRF protection
```

### Startup Sequence (main.rs → bootstrap.rs)

CLI args → config (TOML + `ORION_SECTION__KEY` env overrides) → tracing → metrics → detect DB backend from URL → DB pool + migrations → repositories (workflows, channels, connectors, packages, traces, trace_dlq, audit_logs) → ConnectorRegistry → HTTP client → engine lock (pre-created for channel_call) → cache pool → external pool caches (SQL, MongoDB) → custom functions → Kafka producer (if enabled) → load active channels + workflows → filter by include/exclude patterns → build engine → populate engine lock → reload ChannelRegistry → Kafka consumer (if enabled, config + DB topics merged) → trace queue workers → trace cleanup → DLQ retry → rate limiter → Axum HTTP server → graceful shutdown on SIGTERM/SIGINT.

### Key Architectural Patterns

- **Channels + Workflows:** Channels are service endpoints (sync/async, REST/HTTP/Kafka) that link to workflows. Workflows are versioned task pipelines with JSONLogic conditions. A channel references a workflow via `workflow_id`. The channels, workflows, and connectors of one service form a **package** — the versioned unit `orion-server package` exports/imports between instances (modular monolith; receipts under `/api/v1/admin/packages`, docs in `docs/src/concepts/packages.md`, runnable examples in `examples/packages/`).
- **Repository pattern:** Trait-based (`WorkflowRepository`, `ChannelRepository`, `ConnectorRepository`, `PackageRepository`, `TraceRepository`, `TraceDlqRepository`, `AuditLogRepository`, `ClusterRepository`) with SQL implementations. Traits use `async_trait`. All repos are stored as `Arc<dyn Trait>` in `AppState`.
- **Engine hot-reload:** Engine is held as `Arc<EngineHandle>` wrapping an `ArcSwap<dataflow_rs::Engine>` (`engine/runner.rs`). A reload builds the new engine off to the side and publishes it with one atomic `store` — lock-free; readers finish on the engine they loaded. Reload triggers on status changes (activate/archive), delete, and manually via `POST /api/v1/admin/engine/reload`. Draft creates/updates do not trigger reload. Also rebuilds `ChannelRegistry` and restarts Kafka consumer if topic set changed.
- **Channel registry:** In-memory `ChannelRegistry` (`channel/registry.rs`) holds `ChannelRuntimeConfig` per active channel — parsed config, rate limiters, compiled validation logic, backpressure semaphores, dedup stores, response caches. Has a `RouteTable` for REST route matching (method + path pattern with parameter extraction). Rebuilt on engine reload.
- **Custom async functions:** 10 handlers implement `dataflow_rs::engine::functions::AsyncFunctionHandler`, registered in `engine/handlers.rs::build_custom_functions()` (re-exported from `engine/mod.rs`): `http_call`, `channel_call`, `cache_read`, `cache_write`, `db_read`, `db_write`, `data_query`, `data_write`, `mongo_read`, `publish_kafka`. `data_query`/`data_write` are the portable read/write dialects (backend-neutral filter + envelope → SQL/MongoDB/ES) in `src/query/`; `db_read`/`db_write` are the raw-SQL escape hatch.
- **Connector registry:** In-memory `RwLock<HashMap<String, Arc<ConnectorConfig>>>` with secret masking on API reads, circuit breakers per connector with LRU eviction. Db/es connector configs carry per-operation gates (`operations: { read, insert, update, delete, upsert, raw_write }`, all default `true`) enforced by the data handlers — e.g. set `"delete": false` to make a connector delete-proof.
- **Trace queue:** `tokio::sync::mpsc` channel with semaphore-limited concurrency for async trace processing (`queue/mod.rs`). Failed traces go to DLQ table with automatic retry.
- **Error handling:** `OrionError` enum in `errors.rs` implements `axum::response::IntoResponse`, mapping variants to HTTP status codes. Returns JSON `{"error": {"code": "...", "message": "..."}}`.
- **AppState** (`server/state.rs`): Central shared state struct. Coherent clusters are grouped into sub-structs (R26): `repos` (`storage::repositories::Repositories` — workflows, channels, connectors, packages, traces, trace_dlq, audit_logs), `kafka` (producer, consumer_handle, ingest_status), and `caches` (cache_pool, sql_pool_cache, mongo_pool_cache). Runtime-singular fields stay flat: engine, connector registry, channel registry, trace/audit queues, config, metrics handle, HTTP client, DataLogic instance, rate limit state, readiness flag, cluster runtime, admin-auth failure tracker, trusted proxies. Passed to all route handlers via Axum's `State` extractor.

### Middleware Stack (server/mod.rs)

1. CatchPanicLayer (outermost — panic recovery)
2. OTel trace context extraction (if `tracing.enabled`)
3. HTTP metrics middleware
4. Admin auth middleware (if enabled)
5. Rate limiting middleware (if enabled)
6. Body limit (max payload size)
7. Compression (gzip/brotli)
8. Security headers (CSP, X-Frame-Options, X-Content-Type-Options, Referrer-Policy, Permissions-Policy, HSTS)
9. Request ID layer (generate/propagate x-request-id)
10. Trace layer (request/response tracing)
11. CORS layer

### Request Processing Flow

```
HTTP Request → Axum Router → Data Route Handler
  → Route Resolution (REST pattern match → channel name lookup → fallback)
  → Channel Registry (ingress guards, in order per channel/guards.rs::apply_guards: rate limit, auth, origin, validation, dedup, response cache, backpressure)
  → Engine (RwLock<Arc<Engine>>)
    → Channel Router (match by channel name)
    → Workflow Matcher (JSONLogic condition evaluation + rollout bucket)
    → Task Pipeline (ordered function execution)
  → Response (cache store, JSON response)
```

### API Structure

- **Admin** (`/api/v1/admin/`):
  - **Channels:** CRUD, status management (draft/active/archived; `?dry_run=true` pre-flight, `?reload=defer`), versioning, import/export (`?on_conflict=fail|skip|new_version`), validate, tags (`?tag=` filter). Names are unique per `channel_id`; activation requires an active workflow.
  - **Workflows:** CRUD, status management, versioning, rollout, dry-run test, import/export, validate, `GET /{id}/dependencies` (connector refs + `channel_call` targets)
  - **Connectors:** CRUD (`enabled` flag, tags), reload, test, import/export, validate, circuit breakers (list/reset)
  - **Packages:** promotion receipts — list/get/put; applied versions are content-immutable (same version + different `content_hash` → 409)
  - **Functions:** `GET /functions` — per-function input schemas for tooling
  - **Engine:** status, reload (also batches promotions committed with `?reload=defer`)
  - **Audit logs:** list with filtering; `X-Orion-Change-Context` request header lands in `details`
  - **Backups:** create and list SQLite backups (`VACUUM INTO`, refused in cluster mode). There is **no restore endpoint** — restore is an offline stop/replace-file/start procedure; PostgreSQL and MySQL have no in-product backup and rely on operator snapshot/PITR tooling. See `docs/src/operate/backup-restore.md`.
- **Data** (`/api/v1/data/`): Dynamic handler `/{*path}` — resolves to channel via REST route match or name lookup. Supports sync and async (trailing `/async`). Trace list/get endpoints.
- **Operational:** `GET /health`, `GET /healthz` (liveness), `GET /readyz` (readiness), `GET /metrics`
- **API docs:** `GET /docs` (Swagger UI), `GET /api/v1/openapi.json` — gated by `server.docs.enabled` (unset = served only outside production; 404 when disabled)

### Database

SQLite (default), PostgreSQL, or MySQL — selected at runtime from `storage.url` scheme. All three migration sets are embedded via `sqlx::migrate!()` and the correct set is chosen at startup based on the detected backend (`DbBackend` enum in `storage/mod.rs`). `DbPool` is an enum wrapping the concrete pool types (`SqlitePool`/`PgPool`/`MySqlPool`) with dispatch helpers for query execution. Tables: `workflows` (composite PK `(workflow_id, version)`), `channels` (composite PK `(channel_id, version)`), `connectors`, `packages` (promotion receipts), `traces`, `trace_dlq`, `audit_logs`; workflows, channels and connectors carry a `tags_json` column. Views: `current_workflows`, `current_channels` (latest version per ID). Triggers enforce single-draft-per-ID and active-immutability constraints. Migrations per backend in `migrations/{sqlite,postgres,mysql}/`.

## Testing

- **Integration tests** in `crates/orion-server/tests/integration/`: one binary — each file is a module declared in `tests/integration/main.rs`. Use `common::test_app()` which creates an in-memory SQLite DB, full `AppState`, and Axum router. Tests use `tower::ServiceExt::oneshot()` (no HTTP server needed).
- **Test helpers** in `tests/integration/common/mod.rs`:
  - `test_app()` — returns a ready-to-use `Router` with in-memory DB
  - `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
  - `body_json(response)` — extracts and parses the response body as `serde_json::Value`
- **Pattern for new integration tests:** Clone the app, call `.oneshot(json_request(...))`, assert status, parse body with `body_json()`. See `tests/integration/admin_workflows_test.rs` for examples. Declare the new module in `tests/integration/main.rs`.
- **Other test binaries:** `tests/cluster/` (multi-node contracts), `tests/storage_postgres.rs`, `tests/storage_mysql.rs`, `tests/schema_parity.rs` (container-gated), `tests/metrics_exposition.rs` (isolated for its process-global metrics recorder), plus container-gated modules inside the integration binary listed in `.github/workflows/ci.yml` (kept in sync by `ci_filter_drift_test`).
- **Benchmarks:** `crates/orion-server/tests/benchmark/bench.sh` — 6 scenarios using `hey` HTTP load generator.
- **End-to-end:** `tests/e2e/run.sh` at the repo root — shell suites driving a real server with the CLI binary (`just e2e` locally; the `cli-e2e` CI job). Its data-driven cases split by role: scenario cases in `examples/use-cases/` (deploying the example packages, workflows referenced by file), runtime-behaviour cases in `tests/e2e/cases/`. The full suite-by-suite map of the test estate is `TESTING.md`.

## Configuration

See `crates/orion-server/config.toml.example`. All settings have sensible defaults. Environment variables override via `ORION_SECTION__KEY` format (e.g., `ORION_SERVER__PORT=3000`).

### CLI Commands

```bash
orion-server                              # Start server
orion-server -c config.toml               # Start with config
orion-server validate-config              # Validate config (--format summary for a short view)
orion-server migrate                      # Run migrations
orion-server migrate --dry-run            # Preview migrations
orion-server lint workflow.json           # Strict-validate a workflow JSON file
orion-server dry-run -w wf.json -i in.json --stubs s.json  # Execute a workflow offline with canned connector replies
orion-server test examples/workflow-tests # Run offline *.case.json workflow regression tests
orion-server test-connectivity            # Probe DB (and Kafka if enabled)
orion-server preflight                    # Scan stored channels/workflows for 1.0 breaks
orion-server dump-openapi                 # Print the OpenAPI 3.1 spec
orion-server package <export|lint|plan|apply|diff>  # Promote a package of channels+workflows+connectors between instances
```
