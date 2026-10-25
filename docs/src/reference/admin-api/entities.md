<!-- description: The admin endpoints for the four entity kinds and their receipts: channels, workflows, plugins, connectors and packages. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Entity endpoints

The endpoints that create, version and move the things an instance stores.

| Page | Holds |
|---|---|
| [Channels](./channels.md) | create, validate, activate, version, import and export a channel. |
| [Workflows](./workflows.md) | create, test, activate, roll out, version and read a workflow's dependencies. |
| [Plugins](./plugins.md) | upload a component as JSON or multipart, sign it, version it, activate it. |
| [Connectors](./connectors.md) | create, validate, update, reload, probe, and reset a circuit breaker. |
| [Packages](./packages.md) | read the receipts an instance keeps of what has been applied to it. |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Admin API conventions](./conventions.md): what holds for every one of them.
- [Operational endpoints](./operations.md): the engine, the ledgers and the backups.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
