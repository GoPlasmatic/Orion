# Orion Production Readiness Improvements

A prioritized list of improvements to make Orion production-ready, organized by severity and category. The codebase already has strong foundations (parameterized queries, SSRF protection, circuit breakers, graceful shutdown, Kubernetes probes, structured logging, DLQ, backpressure, etc.) — this document focuses on the remaining gaps.

---

## Critical

### 1. Add security response headers
**File:** `src/server/mod.rs`
No Helmet-style headers are set. Add a middleware layer that sets:
- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- `Strict-Transport-Security: max-age=63072000; includeSubDomains` (when TLS enabled)
- `Content-Security-Policy: default-src 'none'`
- `Referrer-Policy: strict-origin-when-cross-origin`
- `Permissions-Policy: geolocation=(), microphone=(), camera=()`

These are trivial to add via `tower_http::set_header::SetResponseHeaderLayer` or a custom middleware.

### 2. Enable admin auth by default in production
**File:** `src/config/mod.rs` (line ~786)
Admin auth is disabled by default (`enabled: false`). In production mode, startup should **fail** if `admin_auth.enabled` is false or `api_key` is empty — the same way CORS wildcard is rejected in production. Without this, all admin CRUD and engine reload endpoints are unprotected.

### 3. Bound external connector pool caches
**Files:** `src/connector/pool_cache.rs`, `src/connector/redis_pool.rs`, `src/connector/mongo_pool.rs`
SQL, Redis, and MongoDB pool caches are unbounded `HashMap`s with no eviction. If connectors are created/deleted dynamically, these grow forever. Add LRU eviction (similar to the circuit breaker registry) or periodic cleanup of pools for deleted connectors.

---

## High

### 4. Horizontal scaling / multi-instance support
Currently single-instance only. Key issues when running multiple replicas behind a load balancer:
- **Rate limiting** is per-instance (in-memory `DashMap`) — no global rate enforcement.
- **Circuit breaker state** is per-instance — one replica may trip while others don't.
- **Engine reload** via `POST /admin/engine/reload` only affects the targeted instance.
- **Deduplication store** is per-instance — duplicate requests can pass through different replicas.

Options:
- Short-term: document single-instance limitation; use sticky sessions at the LB.
- Medium-term: add Redis-backed rate limiting & dedup (the `cache` connector type already exists).
- Long-term: DB-polling or pub/sub for config change propagation across replicas.

### 5. Database backup & restore tooling
No built-in mechanism for SQLite backup/restore. For production:
- Implement `POST /api/v1/admin/backup` using SQLite's online backup API (`sqlite3_backup_*`).
- Document WAL-mode backup considerations (must copy `.db`, `.wal`, and `.shm` atomically).
- For PostgreSQL/MySQL backends: document `pg_dump`/`mysqldump` procedures.

### 6. Audit log persistence
**File:** `src/server/routes/admin.rs` (line ~15)
Admin mutations (create, update, delete, status changes) are logged via `tracing` but not persisted to the database. Add an `audit_logs` table capturing: timestamp, principal, action, resource_type, resource_id, and before/after snapshots. This is essential for compliance and incident investigation.

### 7. API key rotation mechanism
**File:** `src/server/admin_auth.rs`
Only a single static API key is supported. Production needs:
- Support for multiple concurrent API keys (to allow rotation without downtime).
- Key expiration/revocation.
- Scoped keys (read-only vs. full admin).

### 8. Kafka consumer message gap during engine reload
**File:** `src/server/routes/mod.rs` (lines 250-293)
When topics change during engine reload, the consumer is fully shut down and restarted. Messages arriving between shutdown and startup are missed (they'll be picked up on next poll from last committed offset, but processing latency spikes). Consider:
- Pause/resume instead of stop/start when topic set is unchanged.
- Drain in-flight messages before shutdown.

---

## Medium

### 9. Response streaming for large payloads
**File:** `src/engine/functions/http_common.rs` (line ~149)
HTTP call responses are fully buffered with `response.bytes()`. For large responses (up to the 10 MB limit), this causes memory spikes under concurrency. Implement streaming via `response.chunk()` with size accounting.

### 10. Kafka producer compression and batching
**File:** `src/kafka/producer.rs` (lines 17-28)
No compression or batching is configured. Add:
- `compression.type=lz4` (or `snappy`) for reduced network I/O.
- `linger.ms=5` and `batch.size=65536` for better throughput.
- Explicit `acks=all` for durability guarantees.

### 11. Default Kafka max_inflight too low
**File:** `src/config/mod.rs` (line ~220)
Default `max_inflight` is 10, which severely limits Kafka throughput. Consider raising to 100-500 depending on expected message processing latency. Document tuning guidance.

### 12. Default DB pool size too low for production
**File:** `src/config/mod.rs` (line ~107)
Default `max_connections=10` may bottleneck under concurrent load, especially with trace writes + admin queries. Consider raising the default to 25-50 for SQLite (WAL allows concurrent readers) and documenting pool sizing guidance.

### 13. Wire per-channel response compression config
**File:** `src/channel/mod.rs` (lines 62-65)
Per-channel compression config exists in the data model but is marked as not yet wired into the request pipeline. Either implement it or remove the dead config to avoid confusion.

### 14. Response caching
**File:** `src/channel/mod.rs` (line ~44)
A TODO exists for response caching configuration but it's not implemented. For channels with idempotent GET-like semantics, an in-memory cache (with configurable TTL) would significantly reduce engine load.

### 15. Optimize hot-path header materialization
**File:** `src/server/routes/data.rs` (lines 130-139)
Every request iterates all HTTP headers into a new `HashMap`. Consider:
- Lazy materialization — only build the map if a workflow actually references headers.
- Pre-filter to only include headers the channel's workflows care about.

### 16. Connector pool cache invalidation
**File:** `src/connector/pool_cache.rs`
External database pools created for connectors are cached indefinitely. If a connector's credentials or URL change, the stale pool is still used until process restart. Add cache invalidation on connector update/delete.

### 17. Reduce double serialization on DLQ path
**File:** `src/queue/mod.rs` (lines 334-335, 386-387)
Trace payloads are serialized twice — once for DLQ enqueue and again for trace result storage. Cache the serialized form to avoid redundant work under error conditions.

---

## Low

### 18. Structured deployment documentation
Add a `docs/deployment.md` covering:
- Recommended resource sizing (CPU, memory, disk I/O).
- SQLite WAL mode tuning (checkpoint intervals, page size).
- PostgreSQL/MySQL connection string best practices.
- Reverse proxy configuration (nginx/envoy) with TLS termination.
- Log aggregation setup (ELK, Datadog, etc.).
- Prometheus/Grafana dashboard templates for the exported metrics.
- Runbook for common operational scenarios (engine reload, connector rotation, trace cleanup).

### 19. Database schema migration safety
**Files:** `migrations/sqlite/001_initial.sql`, `src/storage/migration_gen.rs`
Migrations auto-run on startup with no rollback capability. For production:
- Add a `--dry-run-migrations` CLI flag to preview pending migrations without applying.
- Document rollback procedures (manual SQL scripts).
- Consider a migration lock to prevent concurrent migration from multiple instances on PostgreSQL/MySQL.

### 20. OpenTelemetry trace sampling strategy
**File:** `src/server/otel.rs`
Only head-based sampling is supported (`sample_rate` 0.0-1.0). Consider:
- Tail-based sampling (sample errors and slow requests at higher rates).
- Parent-based sampling (respect upstream sampling decisions).

### 21. Graceful degradation under storage failure
If the SQLite database becomes unavailable (disk full, corruption):
- Sync requests currently fail with 500. Consider a read-only degraded mode using the in-memory engine.
- Async requests fail at trace creation. Consider accepting and queuing in-memory with a bounded buffer.

### 22. CLI tooling
Add management subcommands to the `orion-server` binary:
- `orion-server validate-config --config ./config.toml` — validate without starting.
- `orion-server migrate --config ./config.toml` — run migrations only.
- `orion-server export-openapi` — dump the OpenAPI spec to stdout.

### 23. Load testing & benchmarks
No load tests or benchmarks exist. Add:
- `benches/` directory with Criterion benchmarks for engine execution, JSON validation, and route matching.
- A `k6` or `wrk` script for end-to-end load testing with sample workflows.
- Document baseline performance numbers (requests/sec, p99 latency) for reference configurations.

### 24. CORS allows all methods and headers
**File:** `src/server/mod.rs` (lines 117-131)
Even when specific origins are configured, `.allow_methods(Any)` and `.allow_headers(Any)` are used. In production, restrict to the methods and headers actually needed by clients.

### 25. Log-level hot-reload
Changing the log level currently requires a restart. Consider supporting `PUT /api/v1/admin/logging` to dynamically change the tracing filter level at runtime (using `tracing_subscriber::reload::Handle`).

---

## Summary

| Priority | Count | Key Theme |
|----------|-------|-----------|
| Critical | 3 | Security headers, auth enforcement, memory bounds |
| High | 5 | Multi-instance, backup, audit, key rotation, Kafka gaps |
| Medium | 9 | Performance, caching, pool management, dead config |
| Low | 8 | Docs, tooling, testing, operational polish |

**Overall assessment:** The core architecture is solid — error handling, observability, graceful shutdown, input validation, and resilience patterns (circuit breakers, DLQ, backpressure) are production-grade. The critical gaps are around security hardening (response headers, enforced auth) and operational tooling (backup, audit, multi-instance). Addressing the Critical and High items would make Orion confidently deployable in a production environment.
