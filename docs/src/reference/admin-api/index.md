<!-- description: Every Orion admin endpoint: workflows, channels, connectors, plugins, models, packages, engine, audit logs and backups, by resource. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Admin API

Every endpoint under `/api/v1/admin/`, by the resource it addresses, and the contracts they all share.

All admin endpoints are under `/api/v1/admin/`. When [admin authentication](./authentication.md) is enabled, requests must include a valid API key. Success and error bodies follow the [response envelopes](./envelopes.md) below.

| Resource | Operations |
|---|---|
| [Channels](./channels.md) | Create, validate, activate, version, import, and export endpoints |
| [Workflows](./workflows.md) | Create, test, activate, roll out, version, import, and export endpoints |
| [Connectors](./connectors.md) | Create, validate, test, update, and circuit-breaker endpoints |
| [Plugins](./plugins.md) | Upload, activate, version, import, and export WebAssembly plugins |
| [Models](./models.md) | Register, admit, activate, version, import, and export ONNX models held in object storage |
| [Packages](./packages.md) | Inspect applied package receipts |
| [Engine](./engine.md) | Inspect and reload the running engine |
| [Functions](./functions.md) | Discover registered task functions and schemas |
| [Audit logs](./audit-logs.md) | Query administrative actions |
| [Trace DLQ](./trace-dlq.md) | Inspect and retry failed asynchronous persistence |
| [Cron occurrences](./cron-occurrences.md) | Inspect, retry and manually trigger scheduled runs |
| [Backups](./backups.md) | Create and list SQLite backups |

All resources use the [authentication](./authentication.md), [response envelopes](./envelopes.md), and [lifecycle](./lifecycle.md) contracts below. The generated [OpenAPI specification](../openapi.md) is the machine-readable source for clients and code generation.

| Page | Holds |
|---|---|
| [Admin API conventions](./conventions.md) | authentication, envelopes, lifecycle, status changes, export and promotion. |
| [Entity endpoints](./entities.md) | channels, workflows, plugins, models, connectors and packages. |
| [Operational endpoints](./operations.md) | engine, functions, audit logs, trace DLQ, cron occurrences and backups. |

## Related

- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [Promote between environments](../../operate/maintain/promotion.md): the operator's guide to export and import.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
- [The entity lifecycle](../../concepts/lifecycle.md): the rules the status endpoints enforce.
