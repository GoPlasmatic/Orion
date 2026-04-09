# Scalability

Orion handles high-throughput workloads with token-bucket rate limiting, semaphore-based backpressure, async processing queues, and stateless horizontal scaling — all configurable per channel.

## Rate Limiting

Rate limiting operates at two levels: **platform-wide** (all requests) and **per-channel** (individual service endpoints).

**Platform-level** — enable in config:

```toml
[rate_limit]
enabled = true
default_rps = 100
default_burst = 50

[rate_limit.endpoints]
admin_rps = 50
data_rps = 200
```

**Per-channel** — configure in the channel's `config_json`:

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 50
  }
}
```

Rate limiting uses the **token bucket algorithm** — tokens replenish at the configured rate, and burst allows short spikes above the steady-state limit. When the bucket is empty, requests receive `429 Too Many Requests`.

**Per-client keying** — use JSONLogic to compute rate limit keys from request data, enabling per-user or per-tenant limits:

```json
{
  "rate_limit": {
    "requests_per_second": 10,
    "burst": 5,
    "key_logic": { "var": "headers.x-api-key" }
  }
}
```

Rate limiter state is per-instance (in-memory). In multi-instance deployments, divide the configured RPS by the number of instances to approximate global limits, or use sticky sessions at the load balancer.

## Backpressure

Semaphore-based concurrency limits prevent any single channel from overwhelming the system:

```json
{
  "backpressure": {
    "max_concurrent": 200
  }
}
```

When all semaphore permits are taken, additional requests receive `503 Service Unavailable` immediately — this is load shedding. The system sheds excess load rather than queuing unboundedly, which protects latency for requests that are admitted.

Each channel has its own independent backpressure semaphore, so a spike in one channel doesn't affect others.

## Async Processing

For workloads that don't need immediate responses, Orion supports async processing via a bounded trace queue:

```bash
# Submit for async processing — returns immediately with a trace ID
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-123" } }'

# Poll for the result
curl -s http://localhost:8080/api/v1/data/traces/{trace-id}
```

The queue is backed by `tokio::sync::mpsc` channels with configurable concurrency:

```toml
[queue]
workers = 4                       # Concurrent trace workers
buffer_size = 1000                # Channel buffer for pending traces
processing_timeout_ms = 60000     # Per-trace processing timeout
max_result_size_bytes = 1048576   # Max size of trace result (1 MB)
max_queue_memory_bytes = 104857600  # Max memory for queued traces (100 MB)
```

Failed traces go to the **dead letter queue** with automatic retry:

```toml
[queue]
dlq_retry_enabled = true
dlq_max_retries = 5
dlq_poll_interval_secs = 30
```

Completed traces are cleaned up automatically based on retention policy:

```toml
[queue]
trace_retention_hours = 72
trace_cleanup_interval_secs = 3600
```

## Horizontal Scaling

Orion is designed for **single-instance simplicity** with **multi-instance capability**. Each instance is stateless — all persistent data lives in the shared database.

**What works across instances:**

| Component | How It Works |
|-----------|-------------|
| Database | All instances share the same database (PostgreSQL or MySQL recommended) |
| Kafka consumers | Consumer groups handle partition assignment automatically |
| Traces | Stored in the shared database — queries return consistent results |
| Workflows & Channels | Definitions live in the database — all instances load the same set |
| Audit logs | Stored in the shared database regardless of which instance handles the request |

### Per-Instance State

The following components use in-memory state that is local to each instance:

| Component | Impact | Workaround |
|-----------|--------|------------|
| **Rate Limiting** | 3 instances at 100 RPS = 300 RPS effective global limit | Sticky sessions; divide configured RPS by instance count |
| **Request Deduplication** | Same idempotency key on two instances → processed twice | Sticky sessions, or Redis-backed dedup store |
| **Response Caching** | Lower cache hit rates (each instance has a cold cache) | Sticky sessions, or Redis-backed cache connector |
| **Circuit Breakers** | One instance may trip while others keep sending | Acceptable — monitor `/health` on each instance |
| **Engine State** | `POST /admin/engine/reload` only reloads the receiving instance | Script reload to hit all instances (see below) |

**Reload all instances:**

```bash
for host in $INSTANCE_HOSTS; do
  curl -X POST "http://$host:8080/api/v1/admin/engine/reload" \
    -H "Authorization: Bearer $API_KEY"
done
```

Alternatively, use a rolling restart strategy with your orchestrator (e.g., Kubernetes rolling deployment).

### Topology Control

Use channel include/exclude filters to run different Orion instances for different channel groups:

```toml
# Instance A: order processing
[channels]
include = ["orders.*", "payments.*"]

# Instance B: analytics and reporting
[channels]
include = ["analytics.*", "reports.*"]
```

This enables microservice-style deployment where each instance handles a subset of channels, all sharing the same database.

### Database Backend Recommendations

| Backend | Single Instance | Multiple Instances | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Not recommended | WAL mode supports concurrent reads but only one writer. File-based — cannot be shared across hosts. |
| **PostgreSQL** | Supported | Recommended | Full multi-connection support. Use connection pooling (PgBouncer) for many instances. |
| **MySQL** | Supported | Supported | Ensure `READ-COMMITTED` isolation for best concurrency. |

For multi-instance deployments, use PostgreSQL with connection pooling (PgBouncer). Script engine reloads to broadcast to all instances after workflow or channel changes.
