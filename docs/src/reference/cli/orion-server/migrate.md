<!-- description: orion-server migrate runs the database migrations for the configured storage.url without starting the server; --dry-run previews the pending set. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server migrate`

Runs database migrations against the configured `storage.url` without starting the server.

## Synopsis

```bash
orion-server migrate [--dry-run]
```

## Options

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview pending migrations without applying them. |

## Examples

```bash
orion-server -c config.toml migrate --dry-run
```

## Related

- [Server configuration › Storage](../../configuration/storage.md): the `storage.url` the migrations run against.
- [Upgrade an instance](../../../operate/maintain/upgrades.md): where the migration step sits in a rolling upgrade.
- [Deploy with Kubernetes](../../../operate/deploy/kubernetes.md): the pre-upgrade migration Job the chart runs.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
