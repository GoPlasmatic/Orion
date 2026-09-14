<!-- description: The channel endpoints: create, validate, activate, version, import and export, plus the name, route and tag rules each one enforces. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Channel endpoints

The endpoints that create, validate, version and move a channel, and what each refuses.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/channels` | Create channel (as draft). Optional `tags: ["..."]` — selection labels read back by `?tag=` filters and package export |
| GET | `/api/v1/admin/channels` | List channels. Filter with `?status=`, `?channel_type=`, `?protocol=`, `?tag=` |
| GET | `/api/v1/admin/channels/{id}` | Get channel by ID |
| PUT | `/api/v1/admin/channels/{id}` | Update draft channel |
| DELETE | `/api/v1/admin/channels/{id}` | Delete channel (all versions) |
| PATCH | `/api/v1/admin/channels/{id}/status` | Change status (active/archived) — see [Status changes](./status-changes.md) |
| GET | `/api/v1/admin/channels/{id}/versions` | List channel version history |
| POST | `/api/v1/admin/channels/{id}/versions` | Create new draft version from active channel |
| POST | `/api/v1/admin/channels/import` | Bulk import channels (as drafts). `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing id means |
| GET | `/api/v1/admin/channels/export` | Export every matching channel, in the shape `/import` accepts. Filter with `?tag=`, `?status=` |
| POST | `/api/v1/admin/channels/validate` | Validate a channel definition without saving |

`PATCH /{id}/status` on a channel:

- Activation refuses a route another active channel claims.
- Activation refuses a channel whose workflow is missing or not active.
- Activation refuses a name another active channel holds.
- `?dry_run=true` and `?reload=defer` compose as described under
  [Status changes](./status-changes.md).

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Channels](../../concepts/channels.md): what a channel is.
- [Channel configuration](../channel-config/index.md): every key of the `config` these endpoints accept.
- [Status changes](./status-changes.md): activating and archiving one.
