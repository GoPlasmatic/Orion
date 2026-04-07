# Orion Production Readiness Tracker

## Status Legend
- [ ] Not started
- [~] In progress / partial

---

## 1. Security

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 1.3 | CORS lockdown | P1 | [~] | Global default is `CorsLayer::permissive()` when `allowed_origins: ["*"]`. Per-channel CORS implemented. **Remaining:** change global default to reject `*` in production mode. |
| 1.4 | SSRF protection for HTTP connectors | P1 | [ ] | `validation.rs` only checks URL scheme (http/https). No blocklist for internal IPs (169.254.x.x, 10.x.x.x, 127.x.x.x, 192.168.x.x). Workflows can reach metadata endpoints and internal services. |
| 1.5 | Audit logging for admin mutations | P1 | [ ] | Admin routes have `#[tracing::instrument]` spans but no dedicated audit log. No user/principal identity captured. No record of who changed what and when. |
| 1.6 | Metrics endpoint authentication | P2 | [ ] | `/metrics` is unauthenticated. Exposes request counts, error rates, circuit breaker states. Fine internally, risky if exposed. |
| 1.7 | Content-Type enforcement | P2 | [ ] | Server accepts any Content-Type and attempts JSON parsing. Returns 400 on parse failure instead of 415 Unsupported Media Type. |

---

## 2. Reliability & Resilience

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 2.7 | JSONLogic expression complexity limits | P2 | [ ] | No timeout or depth limit on `datalogic_rs` compilation or evaluation. Pathological expressions could DoS. |
| 2.8 | Database query timeouts | P2 | [~] | Pool acquire timeout configurable (`acquire_timeout_secs`, default 5). `DbConnectorConfig` defines `query_timeout_ms` field but it is **never enforced** in query execution. Individual queries can hang indefinitely. |
| 2.9 | Circuit breaker state not durable | P3 | [ ] | In-memory only (`AtomicU8` state machine). LRU eviction at 10,000 entries. Resets on restart. |

---

## 3. Observability

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 3.2 | Database connection pool metrics | P1 | [ ] | Pool created via `sqlx` (SQLite/Postgres/MySQL depending on backend) with no metrics export. No active/idle/waiting connection count gauges. |
| 3.3 | Trace queue depth & worker utilization metrics | P1 | [ ] | Queue uses MPSC (buffer_size=1000) with semaphore (max_workers=4). Neither queue depth nor active worker count exposed as metrics. |
| 3.4 | Kafka consumer lag metric | P1 | [ ] | Consumer commits offsets asynchronously but no lag metric (committed offset vs high water mark) is exported. |
| 3.5 | Engine lock contention metrics | P2 | [ ] | `Arc<RwLock<Arc<Engine>>>` used in multiple paths. No lock wait time measurement or contention counter. |
| 3.6 | Per-connector failure rate metrics | P2 | [~] | `circuit_breaker_trips_total` and `circuit_breaker_rejections_total` counters exist. **Missing:** per-connector request success/failure counters, duration histograms, error type breakdown. |
| 3.7 | Per-task execution time breakdown | P3 | [ ] | `message_duration_seconds` histogram is per-channel only. No per-task (function) timing. |
| 3.8 | Version/build info logged at startup | P3 | [~] | `CARGO_PKG_VERSION` logged at startup and in `/health`. **Missing:** git commit hash, build timestamp, feature flags. |

---

## 4. Data Integrity & Storage

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 4.1 | SQLite backup mechanism | P0 | [ ] | WAL mode enabled with `synchronous=NORMAL` (SQLite backend only). No built-in backup, no explicit WAL checkpoint strategy. N/A for PostgreSQL or MySQL. |
| 4.2 | Trace result size limits | P1 | [ ] | `result_json` stored as unlimited TEXT. Input has 1MB max but workflow results can grow unbounded. |
| 4.3 | Trace queue memory bound | P1 | [~] | Buffer bounded at 1000 entries, concurrency limited by semaphore. **Missing:** no memory accounting for queued items; theoretical max ~1GB. |
| 4.4 | WAL file size limit | P2 | [ ] | No `journal_size_limit` pragma set. WAL can grow unbounded under write-heavy load. SQLite backend only. |
| 4.5 | Database file size monitoring | P2 | [ ] | Health check pings DB connectivity only. No metric for DB file size, table row counts, or disk free space. |
| 4.6 | Async trace DLQ / retry mechanism | P2 | [~] | HTTP async traces have 3 retries with backoff. Kafka failures go to DLQ. **Missing:** no DLQ for HTTP async traces — failed traces just marked `"failed"`. |
| 4.7 | Async trace idempotency | P3 | [ ] | No idempotency key handling. `DeduplicationConfig` exists in channel model but is unwired. |

---

## 5. Kafka Integration

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 5.1 | Kafka integration tests | P0 | [~] | Unit + integration tests added. **Remaining:** partition rebalancing, broker failure, concurrent processing tests. |
| 5.2 | Consumer backpressure | P1 | [ ] | Single-threaded `consume_loop` processes messages sequentially. No `pause()`/`resume()` when engine is overloaded. |
| 5.3 | Consumer lag monitoring | P1 | [ ] | No `kafka_consumer_lag` metric. Cannot observe how far behind the consumer is. |
| 5.4 | DLQ consumer / retry | P2 | [ ] | DLQ producer implemented. **But:** DLQ is write-only. No consumer or retry mechanism. Messages pile up silently. |
| 5.5 | Exactly-once processing guarantee | P2 | [ ] | At-least-once only. Non-transactional commits. No idempotent processing or deduplication window. |
| 5.6 | Consumer config validation at startup | P2 | [~] | Validates brokers, group_id, topic uniqueness. **Missing:** no broker connectivity test at startup. |

---

## 6. Incomplete Features (Defined but Not Wired)

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 6.1 | Channel response caching | P2 | [ ] | `ChannelCacheConfig` struct defined. No cache layer in request handlers. |
| 6.2 | Request deduplication | P2 | [ ] | `DeduplicationConfig` defined. No idempotency key store or lookup in pipeline. |
| 6.3 | Response compression | P2 | [ ] | `CompressionConfig` defined. No gzip/brotli middleware applied. |
| 6.4 | Datastore connectors (SQL read/write) | P1 | [~] | `db_read`/`db_write` in `KNOWN_FUNCTIONS`. **Handlers implemented** (`db_read.rs`, `db_write.rs`) with `SqlPoolCache`. **Not registered** in `build_custom_functions()`. |
| 6.5 | Admin routes in OpenAPI spec | P1 | [ ] | Admin endpoints have `@utoipa::path` annotations but are **not registered** in `ApiDoc` `paths()` macro. |
| 6.6 | Channel include/exclude filtering | P2 | [ ] | `ChannelLoadingConfig` defined in config. **Never referenced** in `reload_engine()`. |
| 6.7 | Cache connectors (Redis read/write) | P2 | [~] | `cache_read`/`cache_write` handlers implemented with `RedisPoolCache`. Feature-gated on `connectors-redis`. **Not registered** in engine, not in `KNOWN_FUNCTIONS`. |
| 6.8 | MongoDB connector (read) | P2 | [~] | `mongo_read` handler implemented with `MongoPoolCache`. Feature-gated on `connectors-mongodb`. **Not registered** in engine, not in `KNOWN_FUNCTIONS`. |

---

## 7. API & Usability

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 7.1 | Connector versioning | P2 | [ ] | Connectors have no version field. Updates are destructive with no rollback. |
| 7.2 | Bulk status change operations | P2 | [ ] | Single-item status change only. No batch activate/archive. |
| 7.3 | Full-text search on names/descriptions | P3 | [ ] | No substring or fuzzy search on name/description fields. |
| 7.4 | Cursor-based pagination | P3 | [ ] | All list endpoints use integer `limit`/`offset` only. |
| 7.5 | Sorting on workflow/channel lists | P3 | [ ] | Fixed sort order. No configurable `sort_by`/`sort_direction` query params. |

---

## 8. Testing Gaps

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 8.1 | Kafka integration tests | P0 | [~] | (Same as 5.1) Remaining: rebalancing, broker failure, concurrency. |
| 8.2 | Concurrency / race condition tests | P1 | [ ] | No tests for concurrent workflow activation, engine reload races, or channel status race conditions. |
| 8.3 | Connection pool exhaustion tests | P1 | [ ] | No tests for pool saturation, timeout behavior, or recovery. |
| 8.4 | Graceful shutdown tests | P2 | [ ] | No tests for in-flight request completion, queue drain timeout, Kafka consumer stop, or signal handling. |

---

## 9. Configuration & Deployment

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 9.1 | Docker feature flag builds | P1 | [~] | Dockerfile uses `cargo build --release --locked` without explicit `--features`. Depends on Cargo.toml defaults, not explicit build config. |
| 9.2 | SQLite data persistence documentation | P1 | [~] | `docs/configuration.md` mentions persistent volumes but no explicit Docker volume mount example. WAL `.wal`/`.shm` sidecar files must also be on persistent volume. |
| 9.3 | Min pool connections config | P2 | [ ] | Only `max_connections` configurable. No `min_connections` or `idle_timeout`. |
| 9.4 | Global HTTP request default timeout | P2 | [~] | `http_call` uses task-level `timeout_ms`. No global default timeout in Orion config. |
| 9.5 | Docker binary stripping | P3 | [ ] | Multi-stage build copies unstripped binary. ~100MB+ with debug symbols. |

---

## Priority Summary

| Priority | Not Started | Partial | Description |
|----------|-------------|---------|-------------|
| **P0** | 1 | 2 | Must fix before production. |
| **P1** | 7 | 6 | Should fix for production hardening. |
| **P2** | 15 | 8 | Important for scale and completeness. |
| **P3** | 5 | 2 | Nice-to-have. |

### P0 Remaining

| # | Item | Status |
|---|------|--------|
| 4.1 | SQLite backup mechanism | [ ] |
| 5.1 / 8.1 | Kafka integration tests | [~] |
