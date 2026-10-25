<!-- description: Every Orion admin endpoint: deployments, engine, circuit breakers, functions, audit logs, cron, backups and cache, by resource. -->
<!-- type: hub -->
<!-- last_verified: 2026-10-25 -->

# Admin API

Every endpoint under `/api/v2/admin/`, by the resource it addresses, and the contracts they all share.

The [deployment endpoints](./deployments.md) are the only way to change what a node serves. Every other resource reads a running node or drives its operational state, such as a breaker reset or a manual cron run. When [admin authentication](./authentication.md) is enabled, every request carries an API key, and every body follows the [response envelopes](./envelopes.md).

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

The generated [OpenAPI specification](../openapi.md) is the machine-readable source for clients and code generation.

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
