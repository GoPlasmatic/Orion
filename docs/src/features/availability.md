# Availability

Orion supports zero-downtime engine reloads, percentage-based canary rollouts, full version lifecycle management, and response caching — enabling continuous delivery without service interruptions.

## Hot-Reload

The engine is held in memory as `Arc<RwLock<Arc<Engine>>>`. A reload swaps the inner `Arc<Engine>` while existing readers continue using the old one — zero dropped requests.

**Trigger a reload:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/engine/reload
```

A reload performs three operations atomically:

1. **Engine swap** — rebuilds the engine from all active workflows and channels in the database
2. **Channel registry rebuild** — reconstructs the route table, validation logic, rate limiters, backpressure semaphores, dedup stores, and response caches
3. **Kafka consumer restart** — if the topic set changed, the Kafka consumer is stopped and restarted with the new topics

Reloads are triggered automatically on status changes (activate/archive) and deletes. Draft creates and updates do not trigger reload.

In multi-instance deployments, reload only affects the instance that receives the request. Script the reload to broadcast to all instances:

```bash
for host in $INSTANCE_HOSTS; do
  curl -X POST "http://$host:8080/api/v1/admin/engine/reload" \
    -H "Authorization: Bearer $API_KEY"
done
```

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

The rollout percentage determines the probability that incoming requests are matched to this workflow. This enables:

- **Gradual migration** — slowly ramp traffic from 0% to 100%
- **A/B testing** — run two workflow versions at different percentages
- **Instant rollback** — set rollout to 0% or archive the workflow

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

**Import and export** — bulk operations for GitOps and migration:

```bash
# Export workflows (as JSON)
curl -s http://localhost:8080/api/v1/admin/workflows/export?status=active

# Import workflows (created as drafts)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/import \
  -H "Content-Type: application/json" -d @workflows.json
```

## Performance

**Response caching** — cache responses for identical requests to reduce redundant workflow execution:

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

Cache keys are computed from the specified fields. Cached responses are returned directly without executing the workflow. The cache backend is in-memory by default; Redis-backed caching is available via a cache connector.

**Request deduplication** — prevent duplicate processing using idempotency keys:

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "retention_secs": 300
  }
}
```

When a request with the same idempotency key arrives within the retention window, it returns `409 Conflict` instead of re-processing.

**Connection pool caching** — external database and MongoDB connector pools are cached and reused across requests, with configurable pool sizes and idle timeouts:

```toml
[engine]
max_pool_cache_entries = 100
cache_cleanup_interval_secs = 60
```
