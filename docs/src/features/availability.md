# Availability

Orion supports zero-downtime engine reloads, percentage-based canary rollouts, full version lifecycle management, and response caching, enabling continuous delivery without service interruptions.

## Hot-Reload

A reload builds the new engine off to the side, then publishes it with one
atomic swap — in-flight requests finish on the engine they started with, and
there is no window in which a reader is held off. Zero dropped requests. The
mechanism is described in
[Design Notes › How hot reload swaps the engine](../reference/design-notes.md#how-hot-reload-swaps-the-engine).

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

**Migrations during deploys:** in cluster mode set `storage.auto_migrate = false` and run `orion-server migrate` as a deploy step before new replicas start (startup fails hard on a pending migration). In production this is not advice: a cluster configured to migrate at boot is refused at startup. Write migrations expand/contract style: first ship a migration that only *adds* (columns, tables, indexes) alongside code that works with both schemas, and only *remove* the old shape in a later release once no running replica depends on it — during a rolling deploy, old and new binaries briefly share one database.

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

**Response caching:** a channel can cache responses for identical requests
and skip workflow execution entirely. The configuration (`cache.enabled`,
`ttl_secs`, `cache_key_fields`), the four-part cache key, and the
no-fields-resolved bypass are specified in
[Channel Configuration › Response caching](../reference/channel-config.md#response-caching).

> [!NOTE]
> Request headers are never part of the cache key, by design. If a response
> varies by anything a header carries, that thing must appear in the payload —
> or the channel must not cache. The reasoning is in
> [Design Notes › Why response-cache keys ignore headers](../reference/design-notes.md#why-response-cache-keys-ignore-headers).

**Request deduplication:** a request repeating an idempotency key inside the
channel's window is answered `409` instead of re-processed; Kafka ingest is
deduplicated too, keyed by record header or record key. Configuration and
semantics are in
[Channel Configuration › Deduplication](../reference/channel-config.md#deduplication).
Duplicate delivery of an unfinished attempt re-runs it; only a settled key
suppresses — deduplication narrows at-least-once, it does not make Kafka
exactly-once. The claim/settle mechanism behind that guarantee is in
[Design Notes › Deduplication: claim, then settle](../reference/design-notes.md#deduplication-claim-then-settle).

**Connection pool caching:** external database and MongoDB connector pools are cached and reused across requests, with configurable pool sizes and idle timeouts:

```toml
[engine]
max_pool_cache_entries = 100
cache_cleanup_interval_secs = 60
```
