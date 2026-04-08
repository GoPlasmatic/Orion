# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a declarative services runtime written in Rust. It exposes business logic management through channels (service endpoints) and workflows (task pipelines powered by dataflow-rs) via a REST API. Ships as a single binary with an embedded SQLite database.

- **Rust Edition:** 2024 (requires Rust 1.85+). The codebase uses let-chains (`if let Some(x) = a && let Some(y) = b`).
- **Core dependencies:** `dataflow-rs` 2.1 (workflow engine), `datalogic-rs` 4 (JSONLogic), `axum` 0.8 (HTTP), `sqlx` 0.8 (database), `sea-query` 0.32 (portable SQL builder)
- **Single binary:** `orion-server` (server, `src/main.rs`)

## Build & Development Commands

```bash
cargo build                        # Build (all features included)
cargo build --release              # Release build

cargo run -- --config ./config.toml  # Run with config file

cargo test                         # Run all tests
cargo test <test_name>             # Run a single test by name

cargo clippy                       # Lint
cargo fmt                          # Format code
```

Docker: `docker build -t orion .` (multi-stage: rust:1.93-slim -> debian:trixie-slim).

## Runtime Configuration

All capabilities are compiled into a single binary — no feature flags. Behavior is controlled at runtime:

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme (`sqlite:`, `postgres://`, `mysql://`) | SQLite |
| Kafka | `kafka.enabled` | Disabled |
| OpenTelemetry | `tracing.enabled` | Disabled |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI | Always at `/docs` | Enabled |
| SQL connectors | `db_read`/`db_write` functions | Always available |
| Redis cache | `cache_read`/`cache_write` with Redis backend | Always available |
| MongoDB connector | `mongo_read` function | Always available |

## Architecture

### Module Structure

```
src/
├── main.rs              # CLI entrypoint, startup sequence
├── lib.rs               # Public module declarations
├── channel/             # Channel registry, config, routing, deduplication
├── config/              # Configuration loading & validation
├── connector/           # Connector types, registry, circuit breakers, pool caching
├── engine/              # Dataflow engine & custom function handlers
│   └── functions/       # http_call, channel_call, db_read/write, cache_read/write, etc.
├── errors.rs            # OrionError enum → HTTP response mapping
├── kafka/               # Kafka producer & consumer
├── metrics/             # Prometheus metrics collection
├── queue/               # Async trace processing, DLQ retry
├── server/              # HTTP server, routes, middleware, state
│   └── routes/          # admin/ (workflows, channels, connectors, engine, audit, backups), data
├── storage/             # Database abstraction, models, repositories
│   └── repositories/    # workflows, channels, connectors, traces, trace_dlq, audit_logs
└── validation/          # Input validation, SSRF protection
```

### Startup Sequence (main.rs)

CLI args → config (TOML + `ORION_SECTION__KEY` env overrides) → tracing → metrics → detect DB backend from URL → DB pool + migrations → repositories (workflows, channels, connectors, traces, audit_logs) → ConnectorRegistry → HTTP client → engine lock (pre-created for channel_call) → cache pool → external pool caches (SQL, MongoDB) → custom functions → Kafka producer (if enabled) → load active channels + workflows → filter by include/exclude patterns → build engine → populate engine lock → reload ChannelRegistry → Kafka consumer (if enabled, config + DB topics merged) → trace queue workers → trace cleanup → DLQ retry → rate limiter → Axum HTTP server → graceful shutdown on SIGTERM/SIGINT.

### Key Architectural Patterns

- **Channels + Workflows:** Channels are service endpoints (sync/async, REST/HTTP/Kafka) that link to workflows. Workflows are versioned task pipelines with JSONLogic conditions. A channel references a workflow via `workflow_id`.
- **Repository pattern:** Trait-based (`WorkflowRepository`, `ChannelRepository`, `ConnectorRepository`, `TraceRepository`, `TraceDlqRepository`, `AuditLogRepository`) with SQL implementations. Traits use `async_trait`. All repos are stored as `Arc<dyn Trait>` in `AppState`.
- **Engine hot-reload:** Engine is `Arc<RwLock<Arc<Engine>>>`. Double-Arc allows swapping the inner engine while readers hold the old one. Reload triggers on status changes (activate/archive), delete, and manually via `POST /api/v1/admin/engine/reload`. Draft creates/updates do not trigger reload. Also rebuilds `ChannelRegistry` and restarts Kafka consumer if topic set changed.
- **Channel registry:** In-memory `ChannelRegistry` (`channel/registry.rs`) holds `ChannelRuntimeConfig` per active channel — parsed config, rate limiters, compiled validation logic, backpressure semaphores, dedup stores, response caches. Has a `RouteTable` for REST route matching (method + path pattern with parameter extraction). Rebuilt on engine reload.
- **Custom async functions:** 8 handlers implement `dataflow_rs::engine::functions::AsyncFunctionHandler`, registered in `engine/mod.rs::build_custom_functions()`: `http_call`, `channel_call`, `cache_read`, `cache_write`, `db_read`, `db_write`, `mongo_read`, `publish_kafka`
- **Connector registry:** In-memory `RwLock<HashMap<String, Arc<ConnectorConfig>>>` with secret masking on API reads, circuit breakers per connector with LRU eviction.
- **Trace queue:** `tokio::sync::mpsc` channel with semaphore-limited concurrency for async trace processing (`queue/mod.rs`). Failed traces go to DLQ table with automatic retry.
- **Error handling:** `OrionError` enum in `errors.rs` implements `axum::response::IntoResponse`, mapping variants to HTTP status codes. Returns JSON `{"error": {"code": "...", "message": "..."}}`.
- **AppState** (`server/state.rs`): Central shared state struct holding engine, all repos, connector registry, cache pool, channel registry, trace queue, config, metrics handle, HTTP client, DataLogic instance, rate limit state, readiness flag, sql_pool_cache, mongo_pool_cache, kafka_consumer_handle, and kafka_producer. Passed to all route handlers via Axum's `State` extractor.

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
  → Channel Registry (dedup check, rate limit, validation, backpressure, cache check)
  → Engine (RwLock<Arc<Engine>>)
    → Channel Router (match by channel name)
    → Workflow Matcher (JSONLogic condition evaluation + rollout bucket)
    → Task Pipeline (ordered function execution)
  → Response (cache store, JSON response)
```

### API Structure

- **Admin** (`/api/v1/admin/`):
  - **Channels:** CRUD, status management (draft/active/archived), versioning
  - **Workflows:** CRUD, status management, versioning, rollout, dry-run test, import/export, validate
  - **Connectors:** CRUD, reload, circuit breakers (list/reset)
  - **Engine:** status, reload
  - **Audit logs:** list with filtering
  - **Backup/Restore:** database export and import
- **Data** (`/api/v1/data/`): Dynamic handler `/{*path}` — resolves to channel via REST route match or name lookup. Supports sync and async (trailing `/async`). Trace list/get endpoints.
- **Operational:** `GET /health`, `GET /healthz` (liveness), `GET /readyz` (readiness), `GET /metrics`
- **API docs:** `GET /docs` (Swagger UI), `GET /api/v1/openapi.json`

### Database

SQLite (default), PostgreSQL, or MySQL — selected at runtime from `storage.url` scheme. All three migration sets are embedded via `sqlx::migrate!()` and the correct set is chosen at startup based on the detected backend (`DbBackend` enum in `storage/mod.rs`). `DbPool` is an enum wrapping the concrete pool types (`SqlitePool`/`PgPool`/`MySqlPool`) with dispatch helpers for query execution. Tables: `workflows` (composite PK `(workflow_id, version)`), `channels` (composite PK `(channel_id, version)`), `connectors`, `traces`, `trace_dlq`, `audit_logs`. Views: `current_workflows`, `current_channels` (latest version per ID). Triggers enforce single-draft-per-ID and active-immutability constraints. Migrations per backend in `migrations/{sqlite,postgres,mysql}/`.

## Testing

- **Integration tests** in `tests/`: Use `common::test_app()` which creates an in-memory SQLite DB, full `AppState`, and Axum router. Tests use `tower::ServiceExt::oneshot()` (no HTTP server needed).
- **Test helpers** in `tests/common/mod.rs`:
  - `test_app()` — returns a ready-to-use `Router` with in-memory DB
  - `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
  - `body_json(response)` — extracts and parses the response body as `serde_json::Value`
- **Pattern for new integration tests:** Clone the app, call `.oneshot(json_request(...))`, assert status, parse body with `body_json()`. See `tests/admin_workflows_test.rs` for examples.
- **Test files:** `admin_workflows_test`, `admin_channels_test`, `admin_connectors_test`, `channel_call_test`, `channel_config_test`, `rest_routing_test`, `rate_limit_test`, `concurrency_test`, `async_traces_test`, `error_paths_test`, `security_test`, `shutdown_test`, `pool_exhaustion_test`, `openapi_test`, `kafka_test`.
- **Benchmarks:** `tests/benchmark/bench.sh` — 6 scenarios using `hey` HTTP load generator.

## Configuration

See `config.toml.example`. All settings have sensible defaults. Environment variables override via `ORION_SECTION__KEY` format (e.g., `ORION_SERVER__PORT=3000`).

### CLI Commands

```bash
orion-server                              # Start server
orion-server -c config.toml               # Start with config
orion-server validate-config              # Validate config
orion-server migrate                      # Run migrations
orion-server migrate --dry-run            # Preview migrations
```
