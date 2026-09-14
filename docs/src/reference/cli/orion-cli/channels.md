<!-- description: orion-cli channels creates, versions, activates, exports and imports channels, and triggers a cron channel now; --dry-run pre-flights an activation. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli channels`

Manages channels. Alias: `ch`.

## Synopsis

```bash
orion-cli channels <subcommand> [args] [flags]
```

## Description

`--dry-run` earns its keep most on channels. Activation requires an active workflow, a route pattern that collides with nothing already serving, and a stored config that still builds. None of those is something a client can check for itself.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List channels; filter with `--status`, `--channel-type`, `--protocol`, `--tag`. Sorts by `priority`, `name`, `status`, `channel_type`, `protocol`, `created_at`, `updated_at`. |
| `get <id>` | Show a channel. |
| `create` | Create a channel from JSON: `-f <file>`, `-d <json>`, or `--stdin`. |
| `update <id>` | Replace a channel definition. Only drafts accept updates. |
| `delete <id>` | Delete a channel; prompts unless `--yes`. |
| `activate <id>` | Activate a draft channel. `--dry-run` pre-flights; `--defer-reload` batches. |
| `archive <id>` | Archive an active channel. Same two flags. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the active one. |
| `validate` | Validate a definition without creating it. Exits `1` when invalid. |
| `export` | Export channels as JSON; filter with `--status`, `--tag`, `--channel-type`, `--protocol`. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |
| `trigger <id>` | Run an active [cron channel](../../channel-config/cron.md) now. Creates an occurrence at the current instant and returns `202`; it takes the same claim and the same singleton a scheduled run does. |

## Examples

```bash
orion-cli channels activate orders --dry-run
```

## Related

- [Channels](../../../concepts/channels.md): what a channel is.
- [Configure a channel](../../../guides/author/channels.md): authoring the definition these subcommands send.
- [Admin API › Channels](../../admin-api/channels.md): the endpoints behind the subcommands.
- [Scheduled workflows](../../../guides/patterns/scheduled-workflows.md): what `trigger` does to a cron channel.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
