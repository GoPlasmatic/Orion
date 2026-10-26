<!-- description: Orion CLI commands for offline authoring and remote administration: compile and test definitions, deploy them, send data and inspect a running node. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# orion-cli

`orion-cli` deploys to a running server and operates it over HTTP: deployments, connections, providers, data, traces and the engine. It also carries the offline authoring tools, which read a definition set on disk and never contact a server. Every server command takes the same [global flags](./global-flags.md), and every `list` pages the same way.

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

## Author offline

These read files and never contact a server. Where one consults the server's configuration — `clippy`'s `[vars]`/`[secrets]` rules, `sql check`'s connection strings, `plugin verify`'s trust keys — `-c, --server-config <path>` names the file `orion-server -c` would read.

| Command | Purpose |
|---|---|
| [`lint`](./lint.md) | Statically validates a workflow JSON file, or a whole definition set, with the checks the server runs on upload |
| [`compile`](./compile.md) | Compiles a definition set into a deployment directory, resolving the authoring conveniences a set may use, such as `$from` for a shared value and `use` for a task fragment |
| [`fmt`](./fmt.md) | Formats definition files to the house style, the way `cargo fmt` formats Rust |
| [`clippy`](./clippy.md) | Advisory checks beyond `lint`, said only when certain — the `cargo clippy` to `lint`'s `cargo check` |
| [`dry-run`](./dry-run.md) | Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace |
| [`test`](./test.md) | Runs a directory of offline workflow test cases |
| [`sql check`](./sql-check.md) | Prepares every `db_read`/`db_write` statement of a set against a real database, as the connector's role |
| [`plugin`](./plugin.md) | Digests, signs and verifies plugin components with the Ed25519 keys `[plugins.trust]` checks |

## Related

- [`orion-server` commands](../orion-server/index.md): the runtime's own subcommands.
- [Admin API](../../admin-api/index.md): the endpoints every command calls.
- [Build your first service](../../../get-started/tutorials/first-service.md): the CLI in use, end to end.
