<!-- description: Every Orion admin endpoint: deployments, engine, circuit breakers, functions, audit logs, cron, backups and cache, by resource. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Admin API

Every endpoint under `/api/v2/admin/`, by the resource it addresses, and the contracts they all share.

All admin endpoints are under `/api/v2/admin/`. When [admin authentication](./authentication.md) is enabled, requests must include a valid API key. Success and error bodies follow the [response envelopes](./envelopes.md) below.

| Resource | Operations |
|---|---|
| [Deployments](./deployments.md) | Store definitions and connection revisions, plan, apply, roll back and follow v2 deployments |
| [Engine](./engine.md) | Inspect what the running engine serves |
| [Circuit breakers](./circuit-breakers.md) | List and reset connector circuit breakers |
| [Functions](./functions.md) | Discover registered task functions and schemas |
| [Audit logs](./audit-logs.md) | Query administrative actions |
| [Trace DLQ](./trace-dlq.md) | Inspect and retry failed asynchronous persistence |
| [Cron occurrences](./cron-occurrences.md) | Inspect, retry and manually trigger scheduled runs |
| [Backups](./backups.md) | Create and list SQLite backups |

All resources use the [authentication](./authentication.md) and [response envelopes](./envelopes.md) contracts below. The generated [OpenAPI specification](../openapi.md) is the machine-readable source for clients and code generation.

| Page | Holds |
|---|---|
| [Admin API conventions](./conventions.md) | authentication and envelopes. |
| [Deployment endpoints](./deployments.md) | definitions, connections, plans, deployments, rollback and convergence. |
| [Circuit breaker endpoints](./circuit-breakers.md) | listing this node's breakers, and resetting one on every node. |
| [Operational endpoints](./operations.md) | engine, functions, audit logs, trace DLQ, cron occurrences, backups and cache. |

## Related

- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [Deployment endpoints](./deployments.md): how definitions reach a node.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
