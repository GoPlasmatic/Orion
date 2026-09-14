<!-- description: orion-cli send posts a payload to a channel, sync or async with --wait, wrapping it in the data envelope unless --raw, with optional metadata and profiling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli send`

Sends data to a channel. Synchronous by default; `--async-mode` submits for background processing and returns a trace ID.

## Synopsis

```bash
orion-cli send <channel> (-f <file> | -d <json> | --stdin) [flags]
```

## Description

By default, `send` takes a **bare business payload** and wraps it in Orion's
`{"data": …}` request envelope. Do not pass an already wrapped envelope: doing
so nests it twice, and a workflow expecting `data.order` would instead receive
`data.data.order`. The same rule applies to `workflows test`.

`--raw` is what reaches a channel configured with
`request.body_mode = "payload"`. Such a channel takes the whole body as `data`,
so the default envelope would arrive as a single key literally named `data`.
`--metadata` is refused alongside it rather than silently dropped: a
payload-mode channel stamps metadata server-side and accepts none from the
caller.

## Options

| Flag | Description |
|------|-------------|
| `<channel>` | Channel name to send data to. |
| `-f, --file <path>` | JSON payload from a file. |
| `-d, --data <json>` | Inline JSON payload. |
| `--stdin` | Read the payload from stdin. |
| `--async-mode` | Submit for async processing; returns a trace ID and trace token. Alias: `--async`. |
| `--wait` | With `--async-mode`, poll until the trace completes. |
| `--timeout <secs>` | Timeout for `--wait`. Default: `60`. |
| `--metadata <json>` | Metadata object attached to the request. Refused with `--raw`. |
| `--raw` | Send the payload as the request body verbatim, with no `{"data": …}` envelope. |
| `--profile` | Request server-side execution profiling; adds an `_orion.profile` breakdown. Sync only, and needs the server's `tracing.debug_profile_enabled`. |

## Examples

```bash
orion-cli send orders -f order.json --async-mode --wait
```

## Related

- [Data API](../../data-api.md): the request path and envelopes behind the command.
- [Channel configuration › Request body](../../channel-config/index.md): the `body_mode` that `--raw` targets.
- [Traces and async processing](../../../operate/run/traces.md): what `--async-mode` returns and how to read it.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
