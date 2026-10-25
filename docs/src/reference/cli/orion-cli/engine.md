<!-- description: orion-cli engine status shows what the engine on this node serves: version, uptime, workflow and channel counts. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli engine`

Shows what the engine on this node serves. Alias: `eng`.

## Synopsis

```bash
orion-cli engine status
```

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `status` | Show engine status: version, uptime, workflow and channel counts. |

## Examples

```bash
orion-cli engine status
```

## Related

- [Admin API › Engine](../../admin-api/engine.md): the endpoint behind the subcommand.
- [Deployment endpoints](../../admin-api/deployments.md): how a node is told what to serve.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
