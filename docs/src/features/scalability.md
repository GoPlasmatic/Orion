# Scalability

Orion handles high-throughput workloads with token-bucket rate limiting, semaphore-based backpressure, async processing queues, and true horizontal scaling: with **cluster mode**, N replicas behind a load balancer behave as a single logical system.

## Rate Limiting

Rate limiting operates at two levels: **platform-wide** (all requests) and **per-channel** (individual service endpoints).

**Platform-level:** enable in config:

```toml
[rate_limit]
enabled = true
default_rps = 100
default_burst = 50

[rate_limit.endpoints]
admin_rps = 50
data_rps = 200
```

**Per-channel:** configure in the channel's `config_json`:

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 50
  }
}
```

Rate limiting uses the **token bucket algorithm**: tokens replenish at the configured rate, and burst allows short spikes above the steady-state limit. When the bucket is empty, requests receive `429 Too Many Requests`.

**Per-channel limits apply on every ingress**, not only HTTP: a synchronous request, an `/async` submission, a Kafka record, and an in-process `channel_call` all meter against the channel's limiter, whether or not the platform limiter (`[rate_limit] enabled`) is on.

They do not, by default, share a *bucket*. The bucket key is the caller identity the transport has — the client IP over HTTP, the topic for Kafka, the calling channel for `channel_call` — which `key_logic` reads as `client_ip` on all four. So `requests_per_second: 100` with no `key_logic` admits 100/s per HTTP client **plus** 100/s from the channel's Kafka topic **plus** 100/s per calling channel: it is a per-caller rate, not a throughput cap on the channel. To get one shared bucket, give `key_logic` an expression that returns the same value on every ingress — `{"var": "channel"}` is the simplest:

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 20,
    "key_logic": { "var": "channel" }
  }
}
```

A Kafka record refused by the limit is *not* dead-lettered: the offset is left uncommitted and the consumer's capped retry backoff becomes the throttle, so throughput is shaped rather than traffic discarded. The one exception is a `key_logic` that cannot be evaluated against the record at all — that is a defect in the expression and will fail identically on every redelivery, so the record is dead-lettered rather than left to block its partition. (Over HTTP both answer `429`.)

Behind a proxy, load balancer, or ingress, set `rate_limit.trusted_proxies` even if you never enable the platform limiter: it is what decides whether `X-Forwarded-For` may name the client, and with it unset every client behind the proxy keys on the proxy's own address and collapses into one bucket. See [`trusted_proxies`](../configuration/reference.md#rate-limiting).

**Per-client keying:** use JSONLogic to compute rate limit keys from request data, enabling per-user or per-tenant limits:

```json
{
  "rate_limit": {
    "requests_per_second": 10,
    "burst": 5,
    "key_logic": { "var": "headers.x-api-key" }
  }
}
```

The key is part of the control, not a hint: a `key_logic` that does not
compile refuses the channel at load (it is quarantined and reported on
`/health`), and a request whose key cannot be evaluated is rejected with
`429`. Neither silently falls back to `client_ip` — that would re-dimension a
per-tenant limit into a per-IP one, sharing a bucket across every tenant
behind one NAT.

**Single instance:** rate limiter state is in-memory (token bucket via governor). Limiter state survives engine reloads: an admin mutation rebuilds the channel registry, but a channel whose limits are unchanged keeps its limiter — consumed burst is not refilled by a reload.

**Cluster mode:** per-channel limits enforce as a shared fixed window on the cluster Redis, so the configured rate holds across **all replicas combined** — 3 replicas at `requests_per_second = 100` is still ~100 RPS globally, and limiter state survives engine reloads. Platform-level limits (`[rate_limit]`, keyed by client IP) intentionally stay per-node: with N replicas the effective platform limit is N× the configured value.

**Backend outages:** in cluster mode the shared limiter depends on Redis. The per-channel `on_backend_error` policy decides what a Redis failure means: `"allow"` (the default) fails open — requests proceed unthrottled until Redis recovers; `"deny"` fails closed — requests are refused with `503 Service Unavailable`. The same policy key exists on `deduplication` (see [Availability](availability.md)):

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 50,
    "on_backend_error": "deny"
  }
}
```

## Backpressure

Semaphore-based concurrency limits prevent any single channel from overwhelming the system:

```json
{
  "backpressure": {
    "max_concurrent_per_node": 200
  }
}
```

When all semaphore permits are taken, additional requests receive `503 Service Unavailable` immediately. This is load shedding. The system sheds excess load rather than queuing unboundedly, which protects latency for requests that are admitted.

Each channel has its own independent backpressure semaphore, so a spike in one channel doesn't affect others. The semaphore is per process — the field is named for that: N replicas admit up to N× `max_concurrent_per_node` in-flight requests in total. (The pre-1.0 name `max_concurrent` is not accepted: it read as a cluster-wide cap, so honouring it under the new field would admit N× the intended concurrency. A stored config using it fails to parse and the channel is quarantined.)

The permit is per **channel**, not per ingress: synchronous requests, queued `/async` work, Kafka records and in-process `channel_call`s all draw from the same semaphore, so `max_concurrent_per_node` bounds the channel's total in-flight work. A Kafka record that cannot get a permit is left uncommitted for redelivery rather than shed, since the transport can wait and the caller cannot be told to.

## Async Processing

For workloads that don't need immediate responses, Orion supports async processing via a bounded trace queue:

```bash
# Submit for async processing (returns immediately with a trace ID)
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-123" } }'

# Poll for the result
curl -s http://localhost:8080/api/v1/admin/traces/{trace-id}
```

The queue is backed by `tokio::sync::mpsc` channels with configurable concurrency:

```toml
[trace_queue]
workers = 4                       # Concurrent trace workers
buffer_size = 1000                # Channel buffer for pending traces
processing_timeout_ms = 60000     # Per-trace processing timeout
max_result_size_bytes = 1048576   # Max size of trace result (1 MB)
max_queue_memory_bytes = 104857600  # Max memory for queued traces (100 MB)
```

Failed traces go to the **dead letter queue** with automatic retry:

```toml
[trace_queue]
dlq_retry_enabled = true
dlq_max_retries = 5
dlq_poll_interval_secs = 30
```

Completed traces are cleaned up automatically based on retention policy:

```toml
[trace_queue]
retention_hours = 72
cleanup_interval_secs = 3600
```

## Horizontal Scaling — Cluster Mode

Orion is designed for **single-instance simplicity** with **first-class multi-instance support**. Enable cluster mode to run N identical replicas behind a load balancer against one shared Postgres/MySQL and one shared Redis:

```toml
[cluster]
enabled = true
redis_url = "redis://redis:6379"   # required; shared dedup / cache / rate limits
epoch_poll_interval_ms = 2000      # how often nodes poll for config changes
instance_id = ""                   # auto-generated UUID when empty

[storage]
url = "postgres://orion:orion@postgres:5432/orion"
auto_migrate = false               # run `orion-server migrate` as a deploy step
```

Cluster mode **requires** Postgres or MySQL storage plus a shared Redis — startup refuses `sqlite:` (single-host by construction). A complete reference topology (2× Orion + Postgres + Redis + nginx) ships as `docker-compose.ha.yml`.

**What cluster mode coordinates:**

| Concern | How it works |
|-----------|-------------|
| **Config changes** | Every admin mutation (workflows, channels, rollout, connectors, manual reload) advances a shared config epoch; every node polls it and resyncs within `epoch_poll_interval_ms`. A change made through *any* node reaches *all* nodes — no fan-out scripting. |
| **Request deduplication** | Channels without an explicit cache connector use the shared cluster Redis — the same idempotency key on two nodes gets exactly one execution and a `409` for the replay. |
| **Response caching** | Shared cluster Redis by default — no per-node cold caches. |
| **Per-channel rate limits** | Shared Redis fixed window — the configured rate holds across all replicas combined. |
| **Background jobs** | Trace cleanup and DLQ retry acquire a per-tick job lease, so only one node runs each job; DLQ entries are additionally row-leased (`FOR UPDATE SKIP LOCKED`), so each entry is retried by exactly one node. |
| **Kafka consumers** | Static group membership (`group.instance.id` = the instance id): rolling restarts rejoin without a full consumer-group rebalance. |
| **Circuit-breaker resets** | `POST /circuit-breakers/{key}` fans out over the epoch bus — one API call resets the key on every node. |

**What intentionally stays per-node** (documented ×N semantics):

| Component | Semantics |
|-----------|-----------|
| **Circuit breakers** | Trip independently per node; each node stops sending after its own `failure_threshold` failures. Resets fan out cluster-wide (above). |
| **Backpressure** | `max_concurrent_per_node` is per node, as named — N replicas admit up to N× the configured value in-flight in total. |
| **Platform rate limits** | `[rate_limit]` IP limits are per node (N× the configured value globally). |
| `/metrics` | Per node — point Prometheus at every replica (or let it discover pods). |

**Guardrails:** in cluster mode, a channel whose dedup/cache connector is missing, broken, or explicitly in-memory **refuses to load** (the activating admin call errors; boot fails) instead of silently degrading to per-node state.

**Filesystem backups are disabled in cluster mode** — `POST /api/v1/admin/backups` returns `400` because the file would land on one arbitrary node (and cluster storage is Postgres/MySQL, which `VACUUM INTO` cannot back up). Use your managed database's native tooling instead: automated snapshots + point-in-time recovery on RDS/Cloud SQL/Azure Database, or `pg_dump` / `mysqldump` on self-managed databases. Redis needs no backup — everything in it (dedup windows, response caches, rate-limit windows) is reconstructible ephemeral state.

### Topology Control

Use channel include/exclude filters to run different Orion instances for different channel groups:

```toml
# Instance A: order processing
[channel_filter]
include = ["orders.*", "payments.*"]

# Instance B: analytics and reporting
[channel_filter]
include = ["analytics.*", "reports.*"]
```

This enables microservice-style deployment where each instance handles a subset of channels, all sharing the same database.

### Database Backend Recommendations

| Backend | Single Instance | Cluster Mode | Notes |
|---------|:-:|:-:|-------|
| **SQLite** | Recommended | Refused at startup | WAL mode supports concurrent reads but only one writer. File-based, cannot be shared across hosts. |
| **PostgreSQL** | Supported | Recommended | Full multi-connection support. Use connection pooling (PgBouncer) for many instances. |
| **MySQL** | Supported | Supported | Ensure `READ-COMMITTED` isolation for best concurrency. |

For cluster deployments, use PostgreSQL with connection pooling (PgBouncer) and set `storage.auto_migrate = false` — run `orion-server migrate` as a deploy step (init container, Helm pre-upgrade hook, or the one-shot `migrate` service in `docker-compose.ha.yml`) so replicas never race migrations at boot. A production cluster left on `auto_migrate = true` is refused at startup rather than allowed to race.
