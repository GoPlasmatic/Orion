<!-- description: The two engine endpoints: reading the running engine's status, and hot-reloading channels and workflows without restarting the process. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Engine endpoints

Reading the running engine, and reloading it.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/engine/status` | Engine status (version, uptime, workflows count, channels) |
| POST | `/api/v1/admin/engine/reload` | Hot-reload channels and workflows |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Status changes](./status-changes.md): the `reload=defer` batches this commits.
- [Engine settings](../configuration/engine.md): the bounds the engine is built with.
- [How Orion works](../../concepts/how-orion-works.md): what a reload republishes.
