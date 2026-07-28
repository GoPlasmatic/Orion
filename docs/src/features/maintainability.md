# Maintainability

Orion provides comprehensive admin APIs, CI/CD integration patterns, dry-run testing, and operational tools for managing services in production.

## Admin APIs

Full CRUD operations for all entities through a RESTful admin API:

| Resource | Endpoints |
|----------|-----------|
| **Workflows** | Create, read, update, delete, status management, versioning, rollout, dry-run test, import/export, validate |
| **Channels** | Create, read, update, delete, status management, versioning |
| **Connectors** | Create, read, update, delete, reload, circuit breaker inspection/reset |
| **Engine** | Status, hot-reload |
| **Audit logs** | List with filtering by action, resource type, resource ID, principal, and time range |
| **Backup** | Create and list SQLite backups (no restore endpoint — see below) |

**Version management:** both workflows and channels support the draft → active → archived lifecycle. Filter by status:

```bash
curl -s "http://localhost:8080/api/v1/admin/workflows?status=active"
curl -s "http://localhost:8080/api/v1/admin/channels?status=draft"
```

**Engine control:**

```bash
# Check engine status
curl -s http://localhost:8080/api/v1/admin/engine/status

# Hot-reload after changes
curl -s -X POST http://localhost:8080/api/v1/admin/engine/reload
```

**OpenAPI / Swagger UI:** interactive API documentation is always available at `/docs`, and the OpenAPI 3.0 spec at `/api/v1/openapi.json`.

## CI/CD Integration

Orion workflows are JSON files that version, diff, and review like any other config.

**Bulk import and export:**

```bash
# Export active workflows
curl -s "http://localhost:8080/api/v1/admin/workflows/export?status=active" -o workflows.json

# Import workflows (created as drafts)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/import \
  -H "Content-Type: application/json" -d @workflows.json
```

**Pre-deploy validation:** validate workflow structure without creating:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/validate \
  -H "Content-Type: application/json" -d @workflow.json
```

**GitOps pipeline:** a typical CI/CD flow:

```
AI generates workflow → commit as JSON → CI validates & dry-runs → review → import → activate
```

GitHub Actions example:

```yaml
name: Validate Workflows
on:
  pull_request:
    paths: ['workflows/**/*.json']

jobs:
  validate:
    runs-on: ubuntu-latest
    services:
      orion:
        image: ghcr.io/goplasmatic/orion:latest
        ports: ['8080:8080']
    steps:
      - uses: actions/checkout@v4
      - name: Import and test workflows
        run: |
          for file in workflows/**/*.json; do
            curl -sf -X POST http://localhost:8080/api/v1/admin/workflows \
              -H "Content-Type: application/json" -d @"$file"
          done
```

**Tag-based organization:** tag workflows for filtering:

```json
{ "tags": ["fraud", "high-priority", "v2"] }
```

```bash
curl -s "http://localhost:8080/api/v1/admin/workflows?tag=fraud"
```

## Testing

**Dry-run execution:** test a workflow against sample data without activating it:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/<id>/test \
  -H "Content-Type: application/json" \
  -d '{"data": {"amount": 50000, "currency": "USD"}}'
```

The response includes a full execution trace showing which tasks ran and which were skipped:

```json
{
  "matched": true,
  "trace": {
    "steps": [
      { "task_id": "parse", "result": "executed" },
      { "task_id": "high_risk", "result": "executed" },
      { "task_id": "normal_risk", "result": "skipped" }
    ]
  },
  "output": {
    "txn": { "amount": 50000, "risk_level": "high", "requires_review": true }
  }
}
```

**Workflow validation:** check that a workflow definition is structurally valid:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/validate \
  -H "Content-Type: application/json" -d @workflow.json
```

**Step-by-step traces:** async traces record the full execution path and can be retrieved for debugging:

```bash
# Submit async request
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" -d '{ "data": { "order_id": "ORD-123" } }'

# Get trace with execution details
curl -s http://localhost:8080/api/v1/admin/traces/{trace-id}
```

## Operations

**Audit logging:** all admin actions are recorded for compliance and debugging:

```bash
curl -s http://localhost:8080/api/v1/admin/audit-logs
curl -s "http://localhost:8080/api/v1/admin/audit-logs?action=status_active&resource_type=workflow"
curl -s "http://localhost:8080/api/v1/admin/audit-logs?resource_id=wf-orders&start_time=2026-07-01T00:00:00Z"
```

Each entry captures: principal, action, resource type, resource ID, details (JSON — currently the originating request ID), and timestamp.

Filters are applied server-side and combine with AND: `action`, `resource_type`,
`resource_id`, `principal` (exact match) plus `start_time` (inclusive) and
`end_time` (exclusive) as RFC 3339 timestamps, on top of `offset` / `limit`.
An unrecognised parameter is rejected with `400` rather than ignored, so a
mistyped filter can never come back as an unfiltered `200`.

Recorded actions are `create`, `update`, `delete`, `import`, `update_rollout`,
`status_active` / `status_archived` / `status_draft`, `reload`, and `backup`.

**Audit retention:** audit rows are only removed by the retention job. It runs
on the `queue.trace_cleanup_interval_secs` cadence and deletes entries older
than `queue.audit_retention_days` (default `90`; set `0` to keep forever). In
cluster mode the job is lease-gated so only one replica performs the delete.

**Database backup:** in-product backup covers **SQLite only**. It is a
`VACUUM INTO` of a consistent copy of the database into a timestamped file in
`storage.backup_dir`, and it is refused in cluster mode (the file would land on
one arbitrary replica).

```bash
# Create a backup — writes storage.backup_dir/orion_backup_<timestamp>.db
curl -s -X POST http://localhost:8080/api/v1/admin/backups

# List the backups currently on this node
curl -s http://localhost:8080/api/v1/admin/backups
```

| Backend | In-product backup | Restore |
|---------|-------------------|---------|
| SQLite | `POST /api/v1/admin/backups` (`VACUUM INTO`), single node only | Stop the server, replace the database file, start it again |
| PostgreSQL | Not provided — use your snapshot/PITR tooling (`pg_dump`, `pg_basebackup`, RDS/Cloud SQL automated backups) | Restore with the same tooling, then start Orion |
| MySQL | Not provided — use your snapshot/PITR tooling (`mysqldump`, binlog PITR, managed-service backups) | Restore with the same tooling, then start Orion |

> There is **no restore endpoint**. Restoring replaces the database Orion is
> actively serving from, so it is an offline operation, not an API call.

**Restore procedure (SQLite):**

```bash
# 1. Stop Orion (SIGTERM drains in-flight requests).
systemctl stop orion          # or: docker compose stop orion

# 2. Put the backup in place of the live database (storage.url path).
cp /var/lib/orion/backups/orion_backup_20260727_101500.db /var/lib/orion/orion.db

# 3. Start Orion. Migrations run at boot unless storage.auto_migrate = false,
#    in which case run `orion-server migrate` first.
systemctl start orion

# 4. Confirm readiness.
curl -sf http://localhost:8080/readyz
```

For PostgreSQL and MySQL the same shape applies — stop the replicas, restore
the snapshot with the database's own tooling, run `orion-server migrate` if
`storage.auto_migrate = false`, then start the replicas.

**Config validation CLI:** validate your configuration without starting the server:

```bash
orion-server validate-config
orion-server validate-config -c config.toml
```

**Database migrations:** run or preview pending migrations:

```bash
orion-server migrate              # Run migrations
orion-server migrate --dry-run    # Preview pending migrations
```
