# Orion Production Readiness Tracker

## Status Legend
- [ ] Not started
- [~] In progress / partial

---

## 1. Security

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 1.6 | Metrics endpoint authentication | P2 | [ ] | `/metrics` is unauthenticated. Exposes request counts, error rates, circuit breaker states. Fine internally, risky if exposed. |

---

## 2. Reliability & Resilience

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 2.7 | JSONLogic expression complexity limits | P2 | [ ] | No timeout or depth limit on `datalogic_rs` compilation or evaluation. Deferred — low practical risk. |
| 2.9 | Circuit breaker state not durable | P3 | [ ] | In-memory only (`AtomicU8` state machine). LRU eviction at 10,000 entries. Resets on restart. |

---

## 3. Observability

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 3.5 | Engine lock contention metrics | P2 | [ ] | `Arc<RwLock<Arc<Engine>>>` used in multiple paths. No lock wait time measurement or contention counter. |
| 3.7 | Per-task execution time breakdown | P3 | [ ] | `message_duration_seconds` histogram is per-channel only. No per-task (function) timing. Needs upstream dataflow-rs changes. |

---

## 4. Data Integrity & Storage

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 4.6 | Async trace DLQ / retry mechanism | P2 | [~] | HTTP async traces have 3 retries with backoff. Kafka failures go to DLQ. **Missing:** no DLQ for HTTP async traces — failed traces just marked `"failed"`. |
| 4.7 | Async trace idempotency | P3 | [ ] | No idempotency key handling. `DeduplicationConfig` exists in channel model but is unwired. |

---

## 5. Kafka Integration

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 5.1 | Kafka integration tests | P0 | [~] | Unit + integration tests added. Concurrent processing, backpressure, multi-topic, and DLQ tests pass. **Remaining:** partition rebalancing, broker failure tests. |
| 5.4 | DLQ consumer / retry | P2 | [ ] | DLQ producer implemented. **But:** DLQ is write-only. No consumer or retry mechanism. Messages pile up silently. |
| 5.5 | Exactly-once processing guarantee | P2 | [ ] | At-least-once only. Non-transactional commits. No idempotent processing or deduplication window. |

---

## 6. Incomplete Features (Defined but Not Wired)

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 6.1 | Channel response caching | P2 | [ ] | `ChannelCacheConfig` struct defined. No cache layer in request handlers. |
| 6.2 | Request deduplication | P2 | [ ] | `DeduplicationConfig` defined. No idempotency key store or lookup in pipeline. |
| 6.3 | Response compression | P2 | [ ] | `CompressionConfig` defined. No gzip/brotli middleware applied. |

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
| 8.1 | Kafka integration tests | P0 | [~] | (Same as 5.1) Concurrency tests added. Remaining: rebalancing, broker failure. |
| 8.4 | Graceful shutdown tests | P2 | [ ] | No tests for in-flight request completion, queue drain timeout, Kafka consumer stop, or signal handling. |

---

## 9. Configuration & Deployment

| # | Item | Priority | Status | Notes |
|---|------|----------|--------|-------|
| 9.3 | Min pool connections config | P2 | [ ] | Only `max_connections` configurable. No `min_connections` or `idle_timeout`. |

---

## Priority Summary

| Priority | Not Started | Partial | Description |
|----------|-------------|---------|-------------|
| **P0** | 0 | 2 | Must fix before production. |
| **P1** | 0 | 0 | All P1 items completed. |
| **P2** | 10 | 1 | Important for scale and completeness. |
| **P3** | 4 | 0 | Nice-to-have. |

### P0 Remaining

| # | Item | Status |
|---|------|--------|
| 5.1 / 8.1 | Kafka integration tests (rebalancing, broker failure) | [~] |
