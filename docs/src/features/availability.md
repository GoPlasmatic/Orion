# Availability

Orion supports zero-downtime engine reloads, percentage-based canary rollouts, full version lifecycle management, and response caching, enabling continuous delivery without service interruptions.

## Hot-Reload

The engine is held in memory as `Arc<RwLock<Arc<Engine>>>`. A reload swaps the inner `Arc<Engine>` while existing readers continue using the old one. Zero dropped requests.

**Trigger a reload:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/engine/reload
```

A reload performs three operations atomically:

1. **Engine swap:** rebuilds the engine from all active workflows and channels in the database
2. **Channel registry rebuild:** reconstructs the route table, validation logic, rate limiters, backpressure semaphores, dedup stores, and response caches
3. **Kafka consumer restart:** if the topic set changed, the Kafka consumer is stopped and restarted with the new topics

Reloads are triggered automatically on status changes (activate/archive), deletes, rollout updates, and connector mutations. Draft creates and updates do not trigger reload.

**Cluster mode:** every mutation also advances a shared config epoch in the database. Each replica polls the epoch (every `cluster.epoch_poll_interval_ms`, default 2 s) and resyncs itself — engine, connector registry, and cached connector pools — when it advances. A change made through *any* node reaches *all* nodes automatically, and `POST /api/v1/admin/engine/reload` is likewise cluster-wide. No broadcast scripting or rolling restart is needed for config changes.

## Canary Rollouts

Control traffic exposure for active workflows with rollout percentages:

```bash
# Activate a workflow at 10% rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<id>/status \
  -H "Content-Type: application/json" \
  -d '{"status": "active", "rollout_percentage": 10}'

# Increase rollout to 50%
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<id>/rollout \
  -H "Content-Type: application/json" \
  -d '{"rollout_percentage": 50}'

# Full rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<id>/rollout \
  -H "Content-Type: application/json" \
  -d '{"rollout_percentage": 100}'
```

The rollout percentage determines the share of traffic matched to this workflow version. This enables:

- **Gradual migration:** slowly ramp traffic from 0% to 100%
- **A/B testing:** run two workflow versions at different percentages
- **Instant rollback:** set rollout to 0% or archive the workflow

**Sticky assignment:** the canary bucket is a hash of a stable caller identity, so the same caller lands on the same version on every request — and on every replica. The identity is the header named by `engine.rollout_sticky_header` (e.g. `x-user-id`) when configured, else the forwarded client IP (`x-forwarded-for` / `x-real-ip`). Direct connections with neither fall back to a random per-request bucket, which still honors the percentages in aggregate.

```toml
[engine]
rollout_sticky_header = "x-user-id"   # optional; default: forwarded client IP
```

## Rolling Deploys

On `SIGTERM`, Orion drains gracefully in a sequence designed for load balancers:

1. `/readyz` flips to **503 immediately** — the LB pulls the node from rotation
2. the node **keeps accepting and serving** for `server.shutdown_drain_secs` (default 30 s), so requests the LB routes here during its own poll interval still succeed
3. accepting stops; in-flight requests get up to `server.shutdown_force_timeout_secs` (default 30 s; `0` = unbounded) to finish

```toml
[server]
shutdown_drain_secs = 30           # readiness-withdrawn grace: still serving
shutdown_force_timeout_secs = 30   # bound on the post-drain in-flight wait
```

Make sure your orchestrator's kill grace exceeds the sum (Kubernetes `terminationGracePeriodSeconds`, compose `stop_grace_period`). Probes: point `readinessProbe` at `/readyz` and `livenessProbe` at `/healthz`.

The `docker-compose.ha.yml` reference topology wires all of this up (2× Orion + Postgres + Redis + nginx), and `deploy/ha/rolling-drill.sh` demonstrates a zero-5xx roll: it drives traffic through the LB while SIGTERM-ing one node and asserts every response was a 2xx.

**Migrations during deploys:** in cluster mode set `storage.auto_migrate = false` and run `orion-server migrate` as a deploy step before new replicas start (startup fails hard on a pending migration). Write migrations expand/contract style: first ship a migration that only *adds* (columns, tables, indexes) alongside code that works with both schemas, and only *remove* the old shape in a later release once no running replica depends on it — during a rolling deploy, old and new binaries briefly share one database.

**Index migrations must not lock the table.** On PostgreSQL a plain `CREATE INDEX` holds a `SHARE` lock for the whole build, which blocks every insert and update — on `traces` that is a write outage as long as the build takes. Index migrations in `migrations/postgres/` therefore begin with the literal `-- no-transaction` marker (sqlx wraps every other migration in a transaction, and `CONCURRENTLY` cannot run inside one) and carry **one statement per file**, because PostgreSQL puts a multi-statement query into an implicit transaction block too. See `migrations/postgres/010_trace_updated_at_index.sql` for the pattern and for what to do if a `CONCURRENTLY` build fails and leaves an `INVALID` index behind. MySQL states `ALGORITHM=INPLACE LOCK=NONE` so an engine that cannot build online fails the migration instead of silently locking; SQLite has no online build and does not need one, being single-node and embedded.

## Versioning

Both workflows and channels follow a **draft → active → archived** lifecycle with automatic version tracking:

```bash
# Create (starts as draft, version 1)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{ "name": "Order Processor", ... }'

# Update (only drafts can be updated)
curl -s -X PUT http://localhost:8080/api/v1/admin/workflows/<id> \
  -H "Content-Type: application/json" -d '{ ... }'

# Activate (loads into engine)
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<id>/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'

# Create new version (new draft from active)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/<id>/versions

# Archive (removes from engine)
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<id>/status \
  -H "Content-Type: application/json" -d '{"status": "archived"}'
```

All versions are stored with incrementing version numbers. List the version history:

```bash
curl -s http://localhost:8080/api/v1/admin/workflows/<id>/versions
```

**Import and export:** bulk operations for GitOps and migration:

```bash
# Export workflows (as JSON)
curl -s http://localhost:8080/api/v1/admin/workflows/export?status=active

# Import workflows (created as drafts)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/import \
  -H "Content-Type: application/json" -d @workflows.json
```

## Performance

**Response caching:** cache responses for identical requests to reduce redundant workflow execution:

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

Cache keys are computed from the specified fields. Cached responses are returned directly without executing the workflow. The cache backend is in-memory by default; Redis-backed caching is available via a cache connector. In cluster mode, channels without an explicit connector use the shared cluster Redis — cache hits are shared across replicas instead of each node warming its own.

**Request deduplication:** prevent duplicate processing using idempotency keys:

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "window_secs": 300
  }
}
```

When a request with the same idempotency key arrives within the window, it returns `409 Conflict` instead of re-processing. Keys are scoped per channel. In cluster mode the dedup store is the shared cluster Redis by default, so the window holds across all replicas.

**Backend outages** are resolved by the channel's `on_backend_error` policy. The default, `"allow"`, fails open: if the dedup store cannot answer (a Redis blip, say), the request proceeds without the idempotency check — availability wins. Payment-style workloads where a duplicate execution is worse than a refused request can set `"on_backend_error": "deny"` to fail closed: the request is refused with `503 Service Unavailable` (never `409` — the key is unverifiable, not a known duplicate) until the backend recovers:

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "window_secs": 300,
    "on_backend_error": "deny"
  }
}
```

**Connection pool caching:** external database and MongoDB connector pools are cached and reused across requests, with configurable pool sizes and idle timeouts:

```toml
[engine]
max_pool_cache_entries = 100
cache_cleanup_interval_secs = 60
```
