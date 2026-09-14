<!-- description: orion-cli backups creates and lists SQLite database backups through the admin API; the other backends rely on their own snapshot tooling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli backups`

Creates and lists database backups. SQLite only.

## Synopsis

```bash
orion-cli backups <create|list>
```

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `create` | Create a backup. |
| `list` | List existing backups. |

## Examples

```bash
orion-cli backups create
```

## Related

- [Back up and restore](../../../operate/maintain/backup-restore.md): the procedure, per backend.
- [Admin API › Backups](../../admin-api/backups.md): the endpoints behind the subcommands.
- [Deploy a cluster](../../../operate/deploy/cluster.md): why the command is refused in cluster mode.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
