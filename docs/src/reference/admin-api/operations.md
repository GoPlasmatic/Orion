<!-- description: The admin endpoints that are not an entity: engine status and reload, the function catalogue, audit logs, the trace DLQ, cron occurrences, backups and response-cache invalidation. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Operational endpoints

The endpoints that read and drive a running instance rather than its stored definitions.

| Page | Holds |
|---|---|
| [Engine](./engine.md) | engine status, and the hot reload. |
| [Functions](./functions.md) | the catalogue of task functions and their input schemas. |
| [Audit logs](./audit-logs.md) | querying the row every admin mutation writes. |
| [Trace DLQ](./trace-dlq.md) | listing, retrying and purging traces the async pipeline could not persist. |
| [Cron occurrences](./cron-occurrences.md) | the durable run ledger, retries, scheduler status and manual triggers. |
| [Backups](./backups.md) | creating and listing SQLite backups. |
| [Cache](./cache.md) | invalidating a channel response-cache namespace. |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Admin API conventions](./conventions.md): what holds for every one of them.
- [Entity endpoints](./entities.md): the stored definitions these operate on.
- [Run an instance](../../operate/index.md): the operator guides that use them.
