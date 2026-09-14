<!-- description: orion-cli engine shows the running engine's status and hot-reloads it from the database, which is how deferred activations are batched into one reload. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli engine`

Controls the engine. Alias: `eng`.

## Synopsis

```bash
orion-cli engine <status|reload>
```

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `status` | Show engine status: version, uptime, workflow and channel counts. |
| `reload` | Hot-reload the engine from the database. |

## Examples

```bash
orion-cli engine reload
```

## Related

- [The entity lifecycle](../../../concepts/lifecycle.md): what a reload publishes.
- [Admin API › Engine](../../admin-api/engine.md): the endpoints behind the subcommands.
- [`orion-cli workflows`](./workflows.md): the `--defer-reload` flag a reload completes.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
