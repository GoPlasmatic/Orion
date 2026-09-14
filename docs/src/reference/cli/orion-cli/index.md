<!-- description: Every orion-cli command: the entity commands for workflows, channels, connectors, plugins and models, plus send, traces, engine, cron, dlq and the rest. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# orion-cli

`orion-cli` manages workflows, channels, connectors, plugins, models, data, traces, and engine operations on a running server over HTTP. Every command takes the same [global flags](./global-flags.md), and every `list` pages the same way.

| Command | Purpose |
|---|---|
| [Global flags](./global-flags.md) | The flags, environment variables and paging controls every command shares |
| [`config`](./config.md) | Manages the CLI's own settings in `~/.orion/config.toml` |
| [`health`](./health.md) | Checks server health, version, and component status |
| [`workflows`](./workflows.md) | Manages workflows |
| [`channels`](./channels.md) | Manages channels |
| [`connectors`](./connectors.md) | Manages connectors and their circuit breakers |
| [`plugins`](./plugins.md) | Manages plugins — WebAssembly components that add task functions |
| [`models`](./models.md) | Manages models — ONNX artifacts held in object storage and admitted before they serve |
| [`send`](./send.md) | Sends data to a channel |
| [`traces`](./traces.md) | Views execution traces |
| [`engine`](./engine.md) | Controls the engine |
| [`functions`](./functions.md) | `list` shows the workflow task functions registered in the engine and their input schemas |
| [`metrics`](./metrics.md) | Fetches `GET /metrics` from the server |
| [`audit-logs`](./audit-logs.md) | `list` shows audit log entries of admin actions |
| [`backups`](./backups.md) | Creates and lists database backups |
| [`packages`](./packages.md) | Inspects package promotion receipts |
| [`dlq`](./dlq.md) | Inspects and drains the trace dead-letter queue |
| [`cron`](./cron.md) | Inspects scheduled runs |
| [`benchmark`](./benchmark.md) | Runs a performance benchmark against the server |
| [`completions`](./completions.md) | Generates shell completions for `bash`, `zsh`, `fish`, `powershell`, or `elvish` |

## Related

- [`orion-server` commands](../orion-server/index.md): the runtime's own subcommands.
- [Admin API](../../admin-api/index.md): the endpoints every command calls.
- [Build your first service](../../../get-started/tutorials/first-service.md): the CLI in use, end to end.
