<!-- description: Every orion-cli command: deployments, connections and providers, plus send, traces, engine, cron, dlq and the rest. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# orion-cli

`orion-cli` deploys to a running server and operates it over HTTP: deployments, connections, providers, data, traces and the engine. Every command takes the same [global flags](./global-flags.md), and every `list` pages the same way.

| Command | Purpose |
|---|---|
| [Global flags](./global-flags.md) | The flags, environment variables and paging controls every command shares |
| [`config`](./config.md) | Manages the CLI's own settings in `~/.orion/config.toml` |
| [`health`](./health.md) | Checks server health, version, and component status |
| [`send`](./send.md) | Sends data to a channel |
| [`traces`](./traces.md) | Views execution traces |
| [`engine`](./engine.md) | Shows what the engine on this node serves |
| [`functions`](./functions.md) | `list` shows the workflow task functions registered in the engine and their input schemas |
| [`metrics`](./metrics.md) | Fetches `GET /metrics` from the server |
| [`audit-logs`](./audit-logs.md) | `list` shows audit log entries of admin actions |
| [`backups`](./backups.md) | Creates and lists database backups |
| [`cache`](./cache.md) | Invalidates channel response-cache namespaces |
| [`dlq`](./dlq.md) | Inspects and drains the trace dead-letter queue |
| [`cron`](./cron.md) | Inspects scheduled runs |
| `deployments` | Plans, applies and rolls back v2 deployments, including `apply -d` for a compiled directory. See [Deployment endpoints](../../admin-api/deployments.md#deploy-a-compiled-directory) |
| `provider` | Authors, lints, packs, uploads, admits and revokes v2 connector provider packages |
| `connections` | Manages immutable v2 connection revisions and runs audited health probes |
| [`completions`](./completions.md) | Generates shell completions for `bash`, `zsh`, `fish`, `powershell`, or `elvish` |

## Related

- [`orion-server` commands](../orion-server/index.md): the runtime's own subcommands.
- [Admin API](../../admin-api/index.md): the endpoints every command calls.
- [Build your first service](../../../get-started/tutorials/first-service.md): the CLI in use, end to end.
