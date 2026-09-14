<!-- description: orion-cli packages lists and shows package promotion receipts, the read side of what orion-server package plan and apply record on an instance. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli packages`

Inspects package promotion receipts. Alias: `pkg`.

## Synopsis

```bash
orion-cli packages <list|get> [name]
```

## Description

Receipts are written by `orion-server package plan/apply`, not by this command:
`orion-cli packages` is the read side, answering which package versions this
instance has staged or applied.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List package receipts, ordered by name and newest first within a package. |
| `get <name>` | Show a package's current receipt and version history. |

## Examples

```bash
orion-cli packages get payments
```

## Related

- [Promote between environments](../../../operate/maintain/promotion.md): what a receipt records.
- [`orion-server package`](../orion-server/package.md): the verbs that write the receipts.
- [Admin API › Packages](../../admin-api/packages.md): the endpoints behind the subcommands.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
