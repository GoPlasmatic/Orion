# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a standalone JSONLogic-based rules engine service written in Rust. It wraps the `dataflow-rs` library and exposes business rule management and data processing through a REST API. Ships as a single binary with an embedded SQLite database.

- **Rust Edition:** 2024 (requires Rust 1.85+ for let-chains)
- **Core dependencies:** `dataflow-rs` (workflow engine), `datalogic-rs` (JSONLogic), `axum` 0.8 (HTTP), `sqlx` 0.8 (SQLite)

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

## Architecture

### Startup Sequence (main.rs)

CLI args -> config (TOML + `ORION_SECTION__KEY` env overrides) -> tracing -> metrics -> SQLite pool (WAL mode) + migrations -> repositories -> ConnectorRegistry -> custom functions -> engine build -> optional Kafka -> job queue workers -> Axum HTTP server -> graceful shutdown on SIGTERM/SIGINT.

### Key Architectural Patterns

- **Repository pattern:** Trait-based (`RuleRepository`, `ConnectorRepository`, `JobRepository`) with SQLite implementations. Traits use `async_trait`.
- **Engine hot-reload:** Engine is `Arc<RwLock<Arc<Engine>>>`. Double-Arc allows swapping the inner engine while readers hold the old one. Reload triggers on any rule CRUD operation or via `POST /api/v1/admin/engine/reload`.
- **Custom async functions:** `HttpCallHandler`, `EnrichHandler`, `PublishKafkaHandler` implement `dataflow_rs::engine::functions::AsyncFunctionHandler`.
- **Connector registry:** In-memory `RwLock<HashMap<String, Arc<ConnectorConfig>>>` with secret masking on API reads.
- **Job queue:** `tokio::sync::mpsc` channel with semaphore-limited concurrency for async job processing.
- **Error handling:** `OrionError` enum implements `axum::response::IntoResponse`, mapping variants to HTTP status codes.

### Request Processing Flow

```
HTTP Request -> Axum Router -> Route Handler
  -> Engine (RwLock<Arc<Engine>>)
    -> Channel Router (match by channel name)
    -> Rule Matcher (JSONLogic condition evaluation)
    -> Task Pipeline (ordered function execution)
  -> JSON Response
```

### API Structure

- **Admin** (`/api/v1/admin/`): Rules CRUD, status management (active/paused/archived), dry-run, import/export, connectors CRUD, engine status/reload
- **Data** (`/api/v1/data/`): `POST /{channel}` (sync), `POST /{channel}/async` + `GET /jobs/{id}` (async), `POST /batch` (batch)
- **Operational:** `GET /health`, `GET /metrics`

### Database

SQLite with WAL mode. Migrations embedded at compile time via `sqlx::migrate!("./migrations")`. Tables: `rules`, `rule_versions` (audit history), `connectors`, `jobs`.

## Testing

- **Integration tests** in `tests/`: Use `common::test_app()` which creates an in-memory SQLite DB and full Axum router. Tests use `tower::ServiceExt::oneshot()` (no HTTP server).
- **Unit tests** inline in: `config/mod.rs`, `errors.rs`, `engine/functions/http_call.rs`, `storage/repositories/rules.rs`.

## Configuration

See `config.toml.example`. All settings have sensible defaults. Environment variables override via `ORION_SECTION__KEY` format (e.g., `ORION_SERVER__PORT=3000`).
