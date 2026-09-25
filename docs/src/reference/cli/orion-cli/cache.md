<!-- description: orion-cli cache invalidate bumps a channel response-cache namespace through the admin API, so every channel declaring it misses on its next request. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `orion-cli cache`

Invalidates channel response-cache namespaces.

## Synopsis

```bash
orion-cli cache invalidate <namespace>
```

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `invalidate <namespace>` | Bump the namespace in every response-cache store. Every channel whose `cache.namespaces` lists it misses on its next request. |

## Examples

```bash
orion-cli cache invalidate ladder
```

## Related

- [`cache`](../../channel-config/cache.md#invalidation): declaring namespaces on a channel.
- [Admin API › Cache](../../admin-api/cache.md): the endpoint behind the subcommand.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
