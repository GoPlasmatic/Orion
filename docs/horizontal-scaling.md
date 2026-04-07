# Horizontal Scaling

Orion is currently designed as a **single-instance** deployment. This document describes what works, what doesn't, and recommended workarounds when running multiple instances behind a load balancer.

## What Works Across Multiple Instances

| Component | How It Works |
|-----------|-------------|
| **Database** | All instances share the same database (SQLite file, PostgreSQL, or MySQL). Repositories are database-backed, so CRUD operations are consistent. |
| **Kafka Consumer** | rdkafka consumer groups handle partition assignment automatically. Multiple instances with the same `group_id` will split partitions across members. |
| **Traces** | Stored in the shared database. Trace queries return consistent results regardless of which instance wrote them. |
| **Workflows & Channels** | Definitions live in the database. All instances load the same active set on startup/reload. |

## Per-Instance State (Not Shared)

The following components use in-memory state that is **local to each instance**:

### Rate Limiting

Rate limiters use an in-memory `DashMap` (via the `governor` crate). Each instance tracks its own counters independently.

**Impact:** If you configure 100 RPS and run 3 instances, the effective limit is 300 RPS globally.

**Workaround:** Use sticky sessions (source IP or header-based affinity) at the load balancer so each client is consistently routed to the same instance. Divide the configured RPS by the number of instances to approximate global limits.

### Request Deduplication

Idempotency key tracking uses an in-memory `DashMap` with TTL-based expiry. Duplicate detection only works within a single instance.

**Impact:** The same idempotency key sent to two different instances will be processed twice.

**Workaround:** Use sticky sessions, or implement application-level deduplication via the shared database.

### Circuit Breakers

Circuit breaker state is tracked per-instance in memory. If an external service fails, one instance may trip its breaker while others continue sending requests.

**Impact:** Degraded but not catastrophic. Each instance independently protects itself. The external service still sees reduced traffic from tripped instances.

**Workaround:** Acceptable for most deployments. For stricter coordination, monitor the `/health` endpoint on each instance (it reports breaker states) and use alerting to detect split-brain scenarios.

### Engine State & Reload

The engine (loaded workflows + channels) is held in memory via `Arc<RwLock<Arc<Engine>>>`. The `POST /api/v1/admin/engine/reload` endpoint only reloads the instance that receives the request.

**Impact:** After modifying a workflow or channel, you must reload **every** instance for the change to take effect.

**Workaround:** Script the reload to hit all instances:
```bash
for host in $INSTANCE_HOSTS; do
  curl -X POST "http://$host:8080/api/v1/admin/engine/reload" \
    -H "Authorization: Bearer $API_KEY"
done
```

Alternatively, use a rolling restart strategy with your orchestrator (e.g., Kubernetes rolling deployment).

### Channel Registry

The channel registry (route table, validation logic, backpressure semaphores) is rebuilt during engine reload. Same per-instance limitation as the engine.

## Recommended Architecture

### Single Instance (Simple)

For most deployments with moderate traffic (< 10k RPS), a single instance with a PostgreSQL or SQLite database is sufficient. Orion's async processing, backpressure, and connection pooling handle concurrency well within a single process.

### Multiple Instances (High Availability)

For high availability without strict global rate limiting:

1. Deploy 2-3 instances behind a load balancer.
2. Use sticky sessions (recommended: `X-Request-ID` header or source IP hash).
3. Configure a shared PostgreSQL database (not SQLite, which doesn't support multi-process writes well).
4. Set the same `kafka.group_id` across instances for automatic partition splitting.
5. Script engine reload to broadcast to all instances.
6. Divide per-channel rate limits by instance count.

### Future: Redis-Backed Distributed State

Orion already supports Redis as a connector type (`cache` connectors with `cache_read`/`cache_write` functions). A future enhancement could use Redis for:

- Distributed rate limiting (Redis sorted sets or token buckets)
- Shared deduplication store (Redis SET with TTL)
- Circuit breaker state coordination
- Engine reload pub/sub notifications

This would enable true horizontal scaling without sticky sessions.

## Database Backend Recommendations

| Backend | Single Instance | Multiple Instances | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Not recommended | WAL mode supports concurrent reads but only one writer. File-based — cannot be shared across hosts. |
| **PostgreSQL** | Supported | Recommended | Full multi-connection support. Use connection pooling (PgBouncer) for many instances. |
| **MySQL** | Supported | Supported | Similar to PostgreSQL. Ensure `READ-COMMITTED` isolation for best concurrency. |
