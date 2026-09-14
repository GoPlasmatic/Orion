<!-- description: orion-cli connectors creates, updates, enables, tests, exports and imports connectors, and lists or resets their circuit breakers; secrets stay masked. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli connectors`

Manages connectors and their circuit breakers. Alias: `conn`.

## Synopsis

```bash
orion-cli connectors <subcommand> [args] [flags]
```

## Description

Connectors are not versioned — there is no draft, no `activate`, and no
`versions`. `update` writes in place and the engine picks it up on reload.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List connectors; filter with `--tag`. Sorts by `name`, `connector_type`, `created_at`, `updated_at`. |
| `get <id>` | Show a connector; secrets stay masked. |
| `create` | Create a connector from JSON: `-f <file>`, `-d <json>`, or `--stdin`. |
| `update <id>` | Replace a connector definition. |
| `delete <id>` | Delete a connector; prompts unless `--yes`. |
| `enable <id>` | Enable a disabled connector. |
| `disable <id>` | Disable a connector without deleting it. |
| `test <id>` | Probe the connector's target with the stored config. An `http` connector's probe is one real request. |
| `validate` | Validate a definition without creating it. Checks the shape only — `test` is what reaches the target. Exits `1` when invalid. |
| `export` | Export connectors as JSON; filter with `--tag`. Secrets stay masked, so a re-import needs them supplied again. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |
| `circuit-breakers` | List circuit breaker states: `closed`, `open`, or `half_open`. |
| `reset-breaker <key>` | Reset a tripped circuit breaker to closed. The key is `connector:channel`. |

## Examples

```bash
orion-cli connectors test payment-api
```

## Related

- [Connectors](../../../concepts/connectors.md): what a connector holds.
- [Connect a database or API](../../../guides/author/connectors.md): creating and testing one.
- [Admin API › Connectors](../../admin-api/connectors.md): the endpoints behind the subcommands.
- [Timeouts, retries and circuit breakers](../../../operate/run/failure-handling.md): what a tripped breaker means.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
