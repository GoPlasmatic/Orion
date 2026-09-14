<!-- description: orion-cli dlq lists, inspects, requeues and purges entries of the trace dead-letter queue, including the exhausted ones nothing retries again. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli dlq`

Inspects and drains the trace dead-letter queue.

## Synopsis

```bash
orion-cli dlq <list|get|requeue|purge> [args] [flags]
```

## Description

`--exhausted true` narrows to entries whose retries are used up — the ones
nothing will pick up again, and the only ones `purge` deletes.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List dead-letter entries; filter with `--channel` and `--exhausted <true\|false>`. |
| `get <id>` | Show one entry, including its payload. |
| `requeue <id>` | Reset an entry's retry counter so the next retry pass picks it up. |
| `purge --older-than-hours <n>` | Permanently delete exhausted entries older than the cut-off. The flag is required, and the command prompts unless `--yes`. |

## Examples

```bash
orion-cli dlq purge --older-than-hours 168
```

## Related

- [Traces and async processing](../../../operate/run/traces.md): how a trace reaches the dead-letter queue.
- [Admin API › Trace DLQ](../../admin-api/trace-dlq.md): the endpoints behind the subcommands.
- [Troubleshooting](../../../operate/maintain/troubleshooting.md): draining a queue that grew.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
