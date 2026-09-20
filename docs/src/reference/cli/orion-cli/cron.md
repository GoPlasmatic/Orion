<!-- description: orion-cli cron shows what each cron channel has scheduled, lists and inspects durable occurrences, and retries a failed or skipped one at its instant. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-20 -->

# `orion-cli cron`

Inspects scheduled runs. Every scheduled instant of a
[cron channel](../../channel-config/cron.md) becomes a durable occurrence. This is the record of what ran, kept whatever the trace-storage settings are.

## Synopsis

```bash
orion-cli cron <status|list|get|retry> [args] [flags]
```

## Description

`retry` keeps the occurrence's identity and its scheduled time. It is another attempt at the work that was due *then*, which is what lets a workflow use
`metadata.trigger.scheduled_for` as an idempotency key. To run a schedule again
*now*, use `channels trigger`, which mints a new occurrence at the current
instant. `completed` occurrences are refused with a `409` for exactly that
reason.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `status` | What is scheduled, when it next fires, its last run, its backlog and its slots, as `held/slots` — one row per active cron channel. |
| `list` | List occurrences, newest first; filter with `--channel-id` and `--status`. |
| `get <id>` | Show one occurrence: both instants, the attempt, the singleton and slot it held, its trace, and why it failed. |
| `retry <id>` | Attempt a `failed`, `skipped_misfire` or `skipped_singleton` occurrence again. |

## Examples

```bash
orion-cli cron list --channel-id nightly-order-rollup --status failed
```

## Related

- [Scheduled workflows](../../../guides/patterns/scheduled-workflows.md): cron channels, occurrences and misfires.
- [Admin API › Cron occurrences](../../admin-api/cron-occurrences.md): the endpoints behind the subcommands.
- [`orion-cli channels`](./channels.md): `trigger`, which mints a new occurrence now.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
