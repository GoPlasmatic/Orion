# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a declarative services runtime written in Rust. It exposes business logic management through channels (service endpoints) and workflows (task pipelines powered by dataflow-rs) via a REST API. Ships as a single binary with an embedded SQLite database.

- **Rust Edition:** 2024 (requires Rust 1.85+). The codebase uses let-chains (`if let Some(x) = a && let Some(y) = b`).
- **Core dependencies:** `dataflow-rs` (workflow engine), `datalogic-rs` (JSONLogic), `axum` 0.8 (HTTP), `sqlx` 0.8 (SQLite)
- **Single binary:** `orion-server` (server, `src/main.rs`)

## Build & Development Commands

```bash
cargo build                        # Build without Kafka
cargo build --features kafka       # Build with Kafka support
cargo build --release              # Release build

cargo run -- --config ./config.toml  # Run with config file

cargo test                         # Run all tests
cargo test --features kafka        # Run tests including Kafka-gated code
cargo test <test_name>             # Run a single test by name

cargo clippy                       # Lint (also run with --features kafka)
cargo fmt                          # Format code
```

Docker: `docker build -t orion .` (multi-stage: rust:1.85-slim -> debian:bookworm-slim)

## Feature Flags

- **`kafka`** (optional): Enables `rdkafka` for Kafka consumer/producer. Without it, `publish_kafka` is a no-op error stub. Gated via `#[cfg(feature = "kafka")]` in `lib.rs`, `main.rs`, `engine/mod.rs`, and `engine/functions/publish_kafka.rs`.
- **`otel`** (optional): Enables OpenTelemetry trace export via OTLP.
- **`swagger-ui`** (optional): Enables Swagger UI at `/docs`.

## Architecture

### Startup Sequence (main.rs)

CLI args -> config (TOML + `ORION_SECTION__KEY` env overrides) -> tracing -> metrics -> SQLite pool (WAL mode) + migrations -> repositories (workflows, channels, connectors, traces) -> ConnectorRegistry -> ChannelRegistry -> engine lock (pre-created for channel_call) -> custom functions -> optional Kafka producer -> load active channels + workflows -> build engine -> populate engine lock -> reload ChannelRegistry -> optional Kafka consumer (config + DB topics merged) -> trace queue workers -> rate limiter -> Axum HTTP server -> graceful shutdown on SIGTERM/SIGINT.

### Key Architectural Patterns

- **Channels + Workflows:** Channels are service endpoints (sync/async, REST/HTTP/Kafka) that link to workflows. Workflows are versioned task pipelines with JSONLogic conditions. A channel references a workflow via `workflow_id`.
- **Repository pattern:** Trait-based (`WorkflowRepository`, `ChannelRepository`, `ConnectorRepository`, `TraceRepository`) with SQLite implementations. Traits use `async_trait`. All repos are stored as `Arc<dyn Trait>` in `AppState`.
- **Engine hot-reload:** Engine is `Arc<RwLock<Arc<Engine>>>`. Double-Arc allows swapping the inner engine while readers hold the old one. Reload triggers on status changes (activate/archive), delete, and manually via `POST /api/v1/admin/engine/reload`. Draft creates/updates do not trigger reload. The reload is in `server/routes/mod.rs::reload_engine()` — it builds the new engine outside the write lock to minimize lock hold time. Also rebuilds `ChannelRegistry` and restarts Kafka consumer if topic set changed.
- **Channel registry:** In-memory `ChannelRegistry` (`channel/mod.rs`) holds `ChannelRuntimeConfig` per active channel — parsed config, rate limiters, compiled validation logic, backpressure semaphores. Has a `RouteTable` for REST route matching (method + path pattern with parameter extraction). Rebuilt on engine reload.
- **Custom async functions:** `HttpCallHandler`, `ChannelCallHandler`, `PublishKafkaHandler` implement `dataflow_rs::engine::functions::AsyncFunctionHandler`. Registered in `engine/mod.rs::build_custom_functions()`.
- **Connector registry:** In-memory `RwLock<HashMap<String, Arc<ConnectorConfig>>>` with secret masking on API reads (`connector/mod.rs::mask_connector_secrets()`).
- **Trace queue:** `tokio::sync::mpsc` channel with semaphore-limited concurrency for async trace processing (`queue/mod.rs`).
- **Error handling:** `OrionError` enum in `errors.rs` implements `axum::response::IntoResponse`, mapping variants to HTTP status codes. Returns JSON `{"error": {"code": "...", "message": "..."}}`.
- **AppState** (`server/state.rs`): Central shared state struct holding engine, all repos (channel_repo, workflow_repo, connector_repo, trace_repo), connector registry, channel registry, trace queue, config, metrics handle, HTTP client, DataLogic instance, rate limit state, and (with kafka feature) Kafka consumer handle + producer. Passed to all route handlers via Axum's `State` extractor.

### Request Processing Flow

```
HTTP Request -> Axum Router -> Data Route Handler
  -> Route Resolution (REST pattern match → channel name lookup → fallback)
  -> Channel Registry (rate limit, validation, backpressure checks)
  -> Engine (RwLock<Arc<Engine>>)
    -> Channel Router (match by channel name)
    -> Workflow Matcher (JSONLogic condition evaluation)
    -> Task Pipeline (ordered function execution)
  -> JSON Response
```

### API Structure

- **Admin** (`/api/v1/admin/`):
  - **Channels:** CRUD, status management (draft/active/archived), versioning
  - **Workflows:** CRUD, status management, versioning, rollout, dry-run test, import/export, validate
  - **Connectors:** CRUD, circuit breakers
  - **Engine:** status, reload
- **Data** (`/api/v1/data/`): Dynamic handler `/{*path}` — resolves to channel via REST route match or name lookup. Supports sync and async (trailing `/async`). Trace list/get endpoints.
- **Operational:** `GET /health`, `GET /metrics`

### Database

SQLite with WAL mode. Migrations embedded at compile time via `sqlx::migrate!("./migrations")`. Tables: `workflows` (with version tracking, composite PK `(workflow_id, version)`), `channels` (with version tracking, composite PK `(channel_id, version)`), `connectors`, `traces`. Views: `current_workflows`, `current_channels` (latest version per ID). Triggers enforce single-draft-per-ID and active-immutability constraints. New migrations go in the `migrations/` directory with sequential numbering (e.g., `002_*.sql`).

## Testing

- **Integration tests** in `tests/`: Use `common::test_app()` which creates an in-memory SQLite DB, full `AppState`, and Axum router. Tests use `tower::ServiceExt::oneshot()` (no HTTP server needed).
- **Test helpers** in `tests/common/mod.rs`:
  - `test_app()` — returns a ready-to-use `Router` with in-memory DB
  - `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
  - `body_json(response)` — extracts and parses the response body as `serde_json::Value`
- **Pattern for new integration tests:** Clone the app, call `.oneshot(json_request(...))`, assert status, parse body with `body_json()`. See `tests/admin_workflows_test.rs` for examples.
- **Unit tests** inline in: `config/mod.rs`, `errors.rs`, `engine/functions/http_call.rs`, `storage/repositories/workflows.rs`.

## Configuration

See `config.toml.example`. All settings have sensible defaults. Environment variables override via `ORION_SECTION__KEY` format (e.g., `ORION_SERVER__PORT=3000`).
