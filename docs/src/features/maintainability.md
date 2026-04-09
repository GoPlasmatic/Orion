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
| **Audit logs** | List with filtering by action and resource type |
| **Backup** | Database export and restore |

**Version management** — both workflows and channels support the draft → active → archived lifecycle. Filter by status:

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

**OpenAPI / Swagger UI** — interactive API documentation is always available at `/docs`, and the OpenAPI 3.0 spec at `/api/v1/openapi.json`.

## CI/CD Integration

Orion workflows are JSON files — they version, diff, and review like any other config.

**Bulk import and export:**

```bash
# Export active workflows
curl -s "http://localhost:8080/api/v1/admin/workflows/export?status=active" -o workflows.json

# Import workflows (created as drafts)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/import \
  -H "Content-Type: application/json" -d @workflows.json
```

**Pre-deploy validation** — validate workflow structure without creating:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/validate \
  -H "Content-Type: application/json" -d @workflow.json
```

**GitOps pipeline** — a typical CI/CD flow:

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

**Tag-based organization** — tag workflows for filtering:

```json
{ "tags": ["fraud", "high-priority", "v2"] }
```

```bash
curl -s "http://localhost:8080/api/v1/admin/workflows?tag=fraud"
```

## Testing

**Dry-run execution** — test a workflow against sample data without activating it:

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

**Workflow validation** — check that a workflow definition is structurally valid:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/validate \
  -H "Content-Type: application/json" -d @workflow.json
```

**Step-by-step traces** — async traces record the full execution path and can be retrieved for debugging:

```bash
# Submit async request
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" -d '{ "data": { "order_id": "ORD-123" } }'

# Get trace with execution details
curl -s http://localhost:8080/api/v1/data/traces/{trace-id}
```

## Operations

**Audit logging** — all admin actions are recorded for compliance and debugging:

```bash
curl -s http://localhost:8080/api/v1/admin/audit-logs
curl -s "http://localhost:8080/api/v1/admin/audit-logs?action=activate&resource_type=workflow"
```

Each entry captures: principal, action, resource type, resource ID, details, and timestamp.

**Database backup and restore:**

```bash
# Export backup
curl -s -X POST http://localhost:8080/api/v1/admin/backup -o backup.json

# Restore from backup
curl -s -X POST http://localhost:8080/api/v1/admin/restore \
  -H "Content-Type: application/json" -d @backup.json
```

**Config validation CLI** — validate your configuration without starting the server:

```bash
orion-server validate-config
orion-server validate-config -c config.toml
```

**Database migrations** — run or preview pending migrations:

```bash
orion-server migrate              # Run migrations
orion-server migrate --dry-run    # Preview pending migrations
```
