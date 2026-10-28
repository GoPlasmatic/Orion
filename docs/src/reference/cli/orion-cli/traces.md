<!-- description: orion-cli traces lists, reads and waits on execution traces, with the trace token an async submission returns and keyset paging for large tables. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli traces`

Views execution traces.

## Synopsis

```bash
orion-cli traces <list|get|wait> [args] [flags]
```

## Description

Reading a trace needs either an admin credential or the per-submission
**trace token** that the async `202` returns alongside the id. Pass it with
`--token <token>` on `get` and `wait`. Without one, any caller who guessed an
id could read another caller's payload.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `list` | List traces, newest first; filter with `--status`, `--channel`, and `--mode`. |
| `get <id>` | Show trace details, including the result or error. |
| `wait <id>` | Poll until the trace completes. `--interval <secs>` (default `1`), `--timeout <secs>` (default `60`). Exit codes: `0` completed, `1` failed, `2` timeout. |

## Options

`list` pages with the flags in [Paging](./global-flags.md#paging):

| Flag | Description |
|------|-------------|
| `--cursor <c>` | Resume after this position: the previous page's `next_cursor`, passed back unmodified. The cost stays flat however deep the loop is. |
| `--include-total` | Ask the server to compute `total`. Off by default: the count is a full scan of the filtered set. |

## Examples

```bash
orion-cli traces list --status failed --channel orders
```

## Related

- [Traces and async processing](../../../operate/run/traces.md): what a trace holds and how it is stored.
- [Data API › Trace endpoints](../../data-api.md#trace-endpoints): the routes and the trace token.
- [`orion-cli dlq`](./dlq.md): the traces that failed to persist.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
