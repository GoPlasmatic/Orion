# Orion Production Readiness Tracker

## Status Legend
- [ ] Not started
- [~] In progress / partial
- [x] Complete

---

## 1. Security (Critical)

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 1.1 | Admin API authentication (bearer token / API key) | P0 | [x] | Bearer token / API-key middleware in `server/admin_auth.rs`. Protects all `/api/v1/admin/*` endpoints. Supports `Authorization: Bearer <token>` and custom header (e.g. `X-API-Key`). Configurable via `[admin_auth]` section or `ORION_ADMIN_AUTH__*` env vars. Constant-time comparison. 401 on missing/invalid. Data, health, metrics endpoints unprotected. 7 integration tests in `security_test.rs`. |
| 1.2 | TLS/HTTPS configuration | P0 | [x] | Feature-gated (`--features tls`) via `axum-server` + rustls. `TlsConfig` struct under `[server.tls]` with `enabled`, `cert_path`, `key_path`. Env overrides: `ORION_SERVER__TLS__*`. Validated at startup. Graceful shutdown via `Handle`. |
| 1.3 | CORS lockdown | P1 | [~] | Global default is `CorsLayer::permissive()` when `allowed_origins: ["*"]` (the default). Per-channel CORS is implemented — `data.rs` checks channel-specific `cors.allowed_origins` and rejects unauthorized origins with 403. Config validation now warns when `*` is used. **Remaining:** change global default to reject `*` in production mode. |
| 1.4 | SSRF protection for HTTP connectors | P1 | [ ] | `validation.rs` only checks URL scheme (http/https). No blocklist for internal IPs (169.254.x.x, 10.x.x.x, 127.x.x.x, 192.168.x.x). Workflows can reach metadata endpoints and internal services. |
| 1.5 | Audit logging for admin mutations | P1 | [ ] | Admin routes have `#[tracing::instrument]` spans but no dedicated audit log. No user/principal identity captured. No record of who changed what and when. |
| 1.6 | Metrics endpoint authentication | P2 | [ ] | `/metrics` is unauthenticated. Exposes request counts, error rates, circuit breaker states. Fine internally, risky if exposed. |
| 1.7 | Content-Type enforcement | P2 | [ ] | Server accepts any Content-Type and attempts JSON parsing. Returns 400 on parse failure instead of 415 Unsupported Media Type. Admin endpoints use Axum `Json` extractor which doesn't enforce Content-Type header either. |

---

## 2. Reliability & Resilience

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 2.1 | `channel_call` cycle detection | P0 | [x] | `_orion_call_depth` and `_orion_call_chain` metadata tracking in `ChannelCallHandler`. Rejects with clear error on depth exceeded or cycle detected. Configurable `max_channel_call_depth` (default: 10) in `EngineConfig`. Internal metadata stripped from child results. |
| 2.2 | `channel_call` timeout isolation | P0 | [x] | `tokio::time::timeout` wraps `process_message_for_channel` at all 3 previously unprotected call sites: `channel_call.rs` (per-call `timeout_ms` or `default_channel_call_timeout_ms`), `queue/mod.rs` (`processing_timeout_ms`), `kafka/consumer.rs` (`processing_timeout_ms`). Kafka timeouts route to DLQ. |
| 2.3 | Panic recovery middleware | P1 | [x] | `CatchPanicLayer` added as outermost middleware in `server/mod.rs`. Custom handler returns JSON error body matching `OrionError` format. Records `metrics::record_error("panic")`. |
| 2.4 | Separate `/ready` and `/live` health endpoints | P1 | [x] | `GET /healthz` (liveness — always 200) and `GET /readyz` (readiness — checks DB, engine lock, and startup `ready` flag). Returns 503 if not ready. Existing `/health` preserved for backward compatibility. |
| 2.5 | Startup readiness gate | P1 | [x] | `Arc<AtomicBool>` `ready` flag in `AppState`. Set to `false` at creation, `true` after engine + channel registry are loaded. Checked by `/readyz`. |
| 2.6 | Graceful shutdown HTTP drain timeout | P2 | [x] | `shutdown_drain_secs` (default: 30) added to `ServerConfig`. Applied to both plain HTTP (sleep after signal in `with_graceful_shutdown`) and TLS (`Handle::graceful_shutdown` with duration) paths. Env override: `ORION_SERVER__SHUTDOWN_DRAIN_SECS`. |
| 2.7 | JSONLogic expression complexity limits | P2 | [ ] | No timeout or depth limit on `datalogic_rs` compilation or evaluation. Used in validation_logic, workflow conditions, and channel_call dynamic routing. Pathological expressions could DoS. |
| 2.8 | Database query timeouts | P2 | [~] | Pool acquire timeout configurable (`acquire_timeout_secs`, default 5). `DbConnectorConfig` defines `query_timeout_ms` field but it is **never enforced** in query execution. Individual queries can hang indefinitely. |
| 2.9 | Circuit breaker state not durable | P3 | [ ] | In-memory only (`AtomicU8` state machine). LRU eviction at 10,000 entries. Resets on restart. Acceptable for most cases but worth noting. |

---

## 3. Observability

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 3.1 | HTTP access logs (request logging middleware) | P0 | [x] | `server/observability.rs` `http_metrics_middleware` now emits structured `tracing::info!` per request with `request_id`, `http.method`, `http.route`, `http.status_code`, `http.response_content_length`, `duration_ms`. Prometheus counters/histograms also recorded. |
| 3.2 | Database connection pool metrics | P1 | [ ] | Pool created via `sqlx::SqlitePool` with no metrics export. No active/idle/waiting connection count gauges. |
| 3.3 | Trace queue depth & worker utilization metrics | P1 | [ ] | Queue uses MPSC (buffer_size=1000) with semaphore (max_workers=4). Neither queue depth nor active worker count exposed as metrics. |
| 3.4 | Kafka consumer lag metric | P1 | [ ] | Consumer commits offsets asynchronously but no lag metric (committed offset vs high water mark) is exported. Silent processing delays invisible. |
| 3.5 | Engine lock contention metrics | P2 | [ ] | `Arc<RwLock<Arc<Engine>>>` used in multiple paths. No lock wait time measurement or contention counter. |
| 3.6 | Per-connector failure rate metrics | P2 | [~] | `circuit_breaker_trips_total` and `circuit_breaker_rejections_total` counters exist per connector+channel. **Missing:** per-connector request success/failure counters, per-connector duration histograms, error type breakdown. |
| 3.7 | Per-task execution time breakdown | P3 | [ ] | `message_duration_seconds` histogram is per-channel only. No per-task (function) timing — cannot identify which step (http_call, filter, map) is the bottleneck. |
| 3.8 | Version/build info logged at startup | P3 | [~] | `CARGO_PKG_VERSION` logged at startup and included in `/health` response. **Missing:** git commit hash, build timestamp, feature flags (no `vergen` or build script). |

---

## 4. Data Integrity & Storage

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 4.1 | SQLite backup mechanism | P0 | [ ] | WAL mode enabled with `synchronous=NORMAL`. No built-in backup, no explicit WAL checkpoint strategy. Relies on SQLite's automatic checkpointing (~1000 pages). |
| 4.2 | Trace result size limits | P1 | [ ] | `result_json` stored as unlimited TEXT. Input has 1MB max (`ingest.max_payload_size`) but workflow results can grow unbounded. Causes DB bloat over time. |
| 4.3 | Trace queue memory bound | P1 | [~] | Buffer bounded at 1000 entries, concurrency limited by semaphore (max_workers=4). Input payload capped at 1MB. **Missing:** no memory accounting for queued items; theoretical max ~1GB. |
| 4.4 | WAL file size limit | P2 | [ ] | No `journal_size_limit` pragma set in `storage/mod.rs`. Only `journal_mode`, `synchronous`, and `cache_size` configured. WAL can grow unbounded under write-heavy load. |
| 4.5 | Database file size monitoring | P2 | [ ] | Health check pings DB connectivity only. No metric or gauge for `orion.db` file size, table row counts, or disk free space. |
| 4.6 | Async trace DLQ / retry mechanism | P2 | [~] | HTTP async traces have 3 retries with 100ms backoff for DB persistence failures (`queue/mod.rs`). Kafka failures go to DLQ topic. **Missing:** no DLQ for HTTP async traces — failed traces just marked `"failed"` in DB. No retry queue. |
| 4.7 | Async trace idempotency | P3 | [ ] | No idempotency key handling. Each submission creates a new trace (UUID PK). `DeduplicationConfig` exists in channel model but is unwired. |

---

## 5. Kafka Integration

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 5.1 | Kafka integration tests | P0 | [~] | Unit tests added: DLQ message format (3 tests), topic-to-channel mapping, config validation (duplicate topics/channels, empty group_id). Integration tests with testcontainers: producer send, consumer lifecycle, valid message processing, invalid JSON → DLQ, metadata injection (6 tests, `#[ignore]` — require Docker). **Remaining:** partition rebalancing, broker failure, concurrent processing tests. |
| 5.2 | Consumer backpressure | P1 | [ ] | Single-threaded `consume_loop` with `recv()` processes messages sequentially inline. No `pause()`/`resume()` on the rdkafka consumer when engine is overloaded. Messages can queue unbounded in librdkafka's internal buffer. |
| 5.3 | Consumer lag monitoring | P1 | [ ] | No `kafka_consumer_lag` metric. Available metrics only cover channel-level `messages_total` and `message_duration_seconds`. Cannot observe how far behind the consumer is. |
| 5.4 | DLQ consumer / retry | P2 | [ ] | DLQ producer implemented — failed messages wrapped with error metadata and published to DLQ topic. **But:** DLQ is write-only. No consumer or retry mechanism. Messages pile up silently. |
| 5.5 | Exactly-once processing guarantee | P2 | [ ] | `enable.auto.commit = false` with `CommitMode::Async`. Non-transactional commits. At-least-once for successful messages. No idempotent processing or deduplication window. |
| 5.6 | Consumer config validation at startup | P2 | [~] | `validate_config()` checks: brokers not empty, group_id not empty, topic uniqueness. **Missing:** no broker connectivity test at startup. Unreachable brokers only error at runtime. |

---

## 6. Incomplete Features (Defined but Not Wired)

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 6.1 | Channel response caching | P2 | [ ] | `ChannelCacheConfig` struct defined with `enabled`, `ttl_secs`, `cache_key_fields`. Marked `TODO: not yet wired into the request pipeline`. No cache layer in request handlers. |
| 6.2 | Request deduplication | P2 | [ ] | `DeduplicationConfig` with `header` name and `window_secs` defined. Marked `TODO: not yet wired into the request pipeline`. No idempotency key store or lookup in pipeline. |
| 6.3 | Response compression | P2 | [ ] | `CompressionConfig` with `enabled` and `min_bytes` defined. Marked `TODO: not yet wired into the request pipeline`. No gzip/brotli middleware applied. |
| 6.4 | Datastore connectors (SQL read/write) | P2 | [ ] | `db_read`/`db_write` listed in `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS` in `engine/mod.rs`. **No handler files exist** — no `src/engine/functions/db.rs`. Not registered in `build_custom_functions()`. Will fail at runtime if referenced. |
| 6.5 | Admin routes in OpenAPI spec | P1 | [ ] | Admin endpoints have `@utoipa::path` annotations but are **not registered** in `ApiDoc` `paths()` macro (`openapi.rs:32-39`). Only data, traces, health, and metrics documented. Confirmed by `openapi_test.rs`. |
| 6.6 | Channel include/exclude filtering | P2 | [ ] | `ChannelLoadingConfig` with `include`/`exclude` glob patterns defined in config. **Never referenced** outside `config/mod.rs`. `reload_engine()` calls `list_active()` with no filtering. Cannot control per-instance channel loading. |

---

## 7. API & Usability

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 7.1 | Connector versioning | P2 | [ ] | Workflows and channels have `version: i64` with full lifecycle. Connectors have no version field — only `id, name, connector_type, config_json, enabled, created_at, updated_at`. Updates are destructive with no rollback. |
| 7.2 | Bulk status change operations | P2 | [ ] | Bulk import exists for workflows. Single-item status change only: `POST /admin/channels/{id}/status`, `POST /admin/workflows/{id}/status`. No batch activate/archive. |
| 7.3 | Full-text search on names/descriptions | P3 | [ ] | Channel filters: status, channel_type, protocol, limit, offset. Workflow filters: status, tag, limit, offset. No substring or fuzzy search on name/description fields. |
| 7.4 | Cursor-based pagination | P3 | [ ] | All list endpoints use integer `limit`/`offset` only. No cursor/token-based pagination. Large datasets will suffer `OFFSET N` performance degradation. |
| 7.5 | Sorting on workflow/channel lists | P3 | [ ] | Channels: `ORDER BY priority DESC` (fixed). Workflows: `ORDER BY priority DESC` or `ORDER BY version DESC` (fixed). No configurable `sort_by`/`sort_direction` query params. Traces support sort params. |

---

## 8. Testing Gaps

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 8.1 | Kafka integration tests | P0 | [~] | (Same as 5.1) Unit + integration tests added. Remaining: rebalancing, broker failure, concurrency. |
| 8.2 | Concurrency / race condition tests | P1 | [ ] | `test_multiple_concurrent_async_traces` submits 10 traces concurrently but only polls completion. No tests for concurrent workflow activation, engine reload races, or channel status race conditions. |
| 8.3 | Connection pool exhaustion tests | P1 | [ ] | Test pool uses max 5 connections. No tests for acquiring all connections, timeout behavior at `acquire_timeout_secs`, or pool recovery after saturation. |
| 8.4 | Graceful shutdown tests | P2 | [ ] | Shutdown sequence documented (5 steps, 30s drain). No tests for in-flight request completion, queue drain timeout enforcement, Kafka consumer stop, or signal handling. |
| 8.5 | Performance / load benchmarks | P2 | [x] | `tests/benchmark/bench.sh` (596 lines) covers: health baseline, simple/complex workflows, multi-workflow channels, concurrency scaling (c=1,10,50,100), reload under load. Measures req/sec, avg/P99 latency, errors. |

---

## 9. Configuration & Deployment

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 9.1 | Docker feature flag builds | P1 | [~] | Dockerfile uses `cargo build --release --locked` without explicit `--features`. **However**, `Cargo.toml` sets `default = ["swagger-ui", "otel", "kafka"]` so Kafka IS included implicitly. Fragile — depends on Cargo.toml defaults, not explicit build config. |
| 9.2 | SQLite data persistence documentation | P1 | [~] | `docs/configuration.md` mentions "configure persistent volume mounts" but no explicit Docker volume mount example. WAL mode creates `.wal` and `.shm` sidecar files that must also be on persistent volume. |
| 9.3 | Min pool connections config | P2 | [ ] | Only `max_connections` configurable in `StorageConfig`. No `min_connections` or `idle_timeout`. SQLx SQLite pool starts with 0 idle connections by default. |
| 9.4 | Global HTTP request default timeout | P2 | [~] | `http_call` uses task-level `timeout_ms` from `FunctionConfig::HttpCall`. No global default timeout in Orion config. If workflow doesn't specify timeout, `dataflow-rs` default applies. Each connector/task must set its own. |
| 9.5 | Docker binary stripping | P3 | [ ] | `cargo build --release --locked` without `strip`. Multi-stage build copies unstripped binary. ~100MB+ with debug symbols. |

---

## Already Production-Ready

These areas are solid and require no immediate work:

| Area | Notes |
|------|-------|
| Engine hot-reload | Arc<RwLock<Arc<Engine>>> with pre-build outside lock. Minimal contention. |
| Workflow versioning & rollout | Draft -> Active -> Archived lifecycle. Canary rollouts via rollout_percentage bucketing. |
| Channel registry | In-memory with rate limiters, validation, backpressure semaphores. Rebuilt on reload. |
| Rate limiting | Platform-level + endpoint-level + per-channel + per-key (JSONLogic). Returns 429 + Retry-After. |
| Circuit breakers | Lock-free atomics. Closed -> Open -> HalfOpen -> Closed. Configurable threshold/cooldown. LRU eviction. |
| Error handling | Comprehensive OrionError enum. Proper HTTP status mapping. Sensitive data hidden. Retryability detection. |
| Connector secret masking | Passwords, tokens, API keys redacted before API responses. |
| Request ID propagation | UUID x-request-id generated/propagated. Available in logs and traces. |
| Graceful shutdown (core) | SIGTERM/SIGINT -> stop HTTP -> stop Kafka -> drain trace workers -> flush OTel. |
| Prometheus metrics | messages_total, errors_total, duration histograms, circuit breaker trips, engine reloads, active workflows. |
| OpenTelemetry (optional) | OTLP export, W3C trace context propagation, configurable sampling. |
| Structured logging | tracing crate with JSON format option. |
| Async processing pipeline | MPSC queue with semaphore-limited workers, retry with backoff, graceful shutdown. |
| Input validation | Per-channel JSONLogic against {data, metadata}. Access to headers, query, path params. |
| Per-channel CORS | Per-channel origin check in data route handler. Rejects unauthorized origins with 403. |
| Per-channel backpressure | Semaphore-based concurrency limiter per channel. Returns 503 when max_concurrent exceeded. |
| Per-channel timeout | tokio::time::timeout wrapper around workflow execution. Configurable per channel via timeout_ms. |
| Workflow dry-run & validation | Isolated test execution. Pre-deploy validation with errors/warnings. |
| Import/export | Bulk workflow import with per-item error reporting. Filtered export. |
| SQL injection protection | All queries use parameterized sqlx bindings. Verified in security tests. |
| REST route matching | RouteTable with method+path pattern matching, path/query param extraction, priority ordering. |
| DB-driven async channels | Kafka consumer merges config-file topics with active async channels from DB. |
| Integration test suite | 11 test files, ~4500 lines. Full HTTP stack with in-memory SQLite. |

---

## Priority Summary

| Priority | Total | Not Started | Partial | Complete | Description |
|----------|-------|-------------|---------|----------|-------------|
| **P0** | 7 | 1 | 1 | 6 | Must fix before production. Security blockers, data loss risks, missing critical tests. |
| **P1** | 16 | 7 | 5 | 4 | Should fix for production hardening. Observability, resilience, operational gaps. |
| **P2** | 19 | 11 | 4 | 2 | Important for scale and completeness. Incomplete features, performance, Kafka gaps. |
| **P3** | 7 | 5 | 1 | 0 | Nice-to-have. UX improvements, minor operational polish. |

### P0 Items (Must Fix)

| # | Item | Status |
|---|------|--------|
| 1.1 | Admin API authentication | [x] |
| 1.2 | TLS/HTTPS configuration | [x] |
| 2.1 | `channel_call` cycle detection | [x] |
| 2.2 | `channel_call` timeout isolation | [x] |
| 3.1 | HTTP access logs | [x] |
| 4.1 | SQLite backup mechanism | [ ] |
| 5.1 / 8.1 | Kafka integration tests | [~] |
