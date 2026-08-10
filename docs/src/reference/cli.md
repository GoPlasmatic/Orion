# CLI Reference

Orion ships two binaries: `orion-server`, the runtime with diagnostic and promotion subcommands, and `orion-cli`, the admin client and MCP server. Both accept `--version`, which prints the version, git hash, and build timestamp.

## orion-server

Running `orion-server` with no subcommand starts the server. The global flag `-c, --config <path>` names the TOML config file and applies to every subcommand. Each subcommand loads the same merged configuration: defaults, then the file, then `ORION_*` environment overrides. See [How Settings Are Resolved](./configuration.md#how-settings-are-resolved).

```bash
orion-server                          # Start with defaults
orion-server -c config.toml           # Start with a config file
```

### `validate-config`

Validates the configuration without starting the server, then prints the full effective config with secrets masked. Exits non-zero on an invalid value.

```bash
orion-server validate-config [--format <toml|json|summary>]
```

| Flag | Description |
|------|-------------|
| `--format` | Output format: `toml` (default), `json`, or `summary` (a short human summary). |

Example: `orion-server -c config.toml validate-config --format summary`

### `migrate`

Runs database migrations against the configured `storage.url` without starting the server.

```bash
orion-server migrate [--dry-run]
```

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview pending migrations without applying them. |

Example: `orion-server -c config.toml migrate --dry-run`

### `lint`

Statically validates a workflow JSON file with the same checks the admin `POST /workflows` endpoint runs. Exits non-zero with field-pathed errors, so it can gate CI. Needs no config, database, or server.

```bash
orion-server lint <workflow.json>
```

Example: `orion-server lint examples/workflows/enrich-order.json`

### `dry-run`

Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace. Connector-backed tasks are answered from `--stubs`; without a matching stub the task fails and names the stub it needs.

```bash
orion-server dry-run -w <workflow.json> -i <input.json> [--stubs <stubs.json>]
```

| Flag | Description |
|------|-------------|
| `-w, --workflow` | Path to a workflow JSON file. |
| `-i, --input` | Path to a JSON file used as the message payload. |
| `-s, --stubs` | Path to a JSON file of canned connector responses. The inner key is the task's `connector` (or `channel` for `channel_call`); `"*"` matches any. |

Example: `orion-server dry-run -w wf.json -i input.json --stubs stubs.json`

### `test`

Runs a directory of offline workflow test cases. Each `*.case.json` file names a workflow, an input, optional connector stubs, and expected output values. Prints a per-case diff and exits non-zero on any failure.

```bash
orion-server test <path>
```

| Argument | Description |
|----------|-------------|
| `path` | A directory of `*.case.json` files, or a single case file. Paths inside a case resolve relative to the case file. |

Example: `orion-server test examples/workflow-tests`

### `test-connectivity`

Probes the configured database with a no-op query, and Kafka when `kafka.enabled = true`. Catches wrong credentials before the server tries to start.

Example: `orion-server -c config.toml test-connectivity`

### `preflight`

Scans stored channels and workflows for anything the 1.0 rules refuse: configs that no longer parse, tasks the validator rejects, and `data_query`/`data_write` tasks with no `schema`. Read-only; exits non-zero on findings. Config-file problems are `validate-config`'s job — this reads what only the database knows.

Example: `orion-server -c config.toml preflight`

### `dump-openapi`

Prints the public HTTP API's OpenAPI 3.1 spec as JSON to stdout. Needs no config, database, or running server. See [OpenAPI](./openapi.md).

Example: `orion-server dump-openapi > openapi.json`

### `package`

Exports a package — selected channels, their workflows, and every connector those workflows reference — and promotes it between instances. The artifact is one JSON document. Every subcommand except `lint` calls an instance's admin API, authenticating with the `ORION_ADMIN_TOKEN` environment variable. The model is described in [Packages & Promotion](../topology/packages.md).

```bash
orion-server package <export|lint|plan|apply|diff> [flags]
```

| Subcommand | Description |
|------------|-------------|
| `export` | Compute the dependency closure from a running instance and write the artifact. |
| `lint` | Validate an artifact offline: entity shapes, closure completeness, content hash. Exits non-zero on findings. |
| `plan` | Pre-flight an artifact against a target with zero writes. |
| `apply` | Stage all entities, activate in dependency order, reload once, record the receipt. Idempotent. |
| `diff` | Report drift between an artifact and a running instance. Exits non-zero when anything differs. |

| Flag | Used by | Description |
|------|---------|-------------|
| `-s, --server <url>` | `export`, `plan`, `apply`, `diff` | Base URL of the source or target instance. |
| `-f, --file <path>` | `lint`, `plan`, `apply`, `diff` | Path to the artifact file. |
| `--tag <tag>` | `export` | Select every channel carrying this tag. |
| `--channels <ids>` | `export` | Select channels by id, comma-separated or repeated. |
| `--name <name>` | `export` | Package name. |
| `--version <ver>` | `export` | Package version. Applied versions are immutable; any content change needs a bump. |
| `-o, --output <path>` | `export` | Write the artifact here instead of stdout. |

Example: `orion-server package export -s https://dev.orion.internal --tag payments --name payments --version 1.4.0 -o pkg.json`

## orion-cli

`orion-cli` manages workflows, channels, connectors, data, traces, and engine operations on a running server over HTTP. Settings resolve in precedence order: CLI flags, then environment variables, then `~/.orion/config.toml`.

Global flags apply to every subcommand:

| Flag | Description |
|------|-------------|
| `--server <url>` | Orion server URL. Overrides the config file and `ORION_SERVER_URL`. |
| `--api-key <key>` | API key for admin authentication. |
| `--api-key-header <name>` | Header name carrying the key. Default: `Authorization` with a `Bearer` prefix. |
| `--output <format>` | Output format: `table` (default), `json`, or `yaml`. |
| `--quiet` | Print only IDs or minimal info. |
| `--verbose` | Show full response bodies and extra details. |
| `--no-color` | Disable colored output. |
| `--yes` | Skip confirmation prompts. |

### `config`

Manages the CLI's own settings in `~/.orion/config.toml`.

| Subcommand | Description |
|------------|-------------|
| `set-server <url>` | Set the Orion server URL. |
| `show` | Show the current CLI configuration. |
| `get <key>` | Print a single value, for scripting. |
| `set <key> <value>` | Set a value: `server_url`, `default_output`, `api_key`, or `api_key_header`. |

Example: `orion-cli config set-server http://localhost:8080`

### `health`

Checks server health, version, and component status. Exits `1` when any component is degraded.

Example: `orion-cli health`

### `workflows`

Manages workflows. Alias: `rules`.

| Subcommand | Description |
|------------|-------------|
| `list` | List workflows; filter with `--status` and `--tag`. |
| `get <id>` | Show a workflow; `--verbose` includes condition and tasks. |
| `create` | Create a workflow from JSON. |
| `update <id>` | Replace a workflow definition. |
| `delete <id>` | Delete a workflow; prompts unless `--yes`. |
| `activate <id>` | Activate a draft workflow. |
| `archive <id>` | Archive an active workflow. |
| `dependencies <id>` | Show the workflow's connectors and `channel_call` targets. |
| `validate` | Validate a definition without creating it. |
| `rollout <id>` | Update the rollout percentage. |
| `versions <id>` | List version history. |
| `new-version <id>` | Create a new draft version. |
| `test <id>` | Dry-run the workflow with sample data. |
| `export` | Export workflows as JSON. |
| `import` | Bulk-import from a JSON array file; `--dry-run` previews. |
| `diff` | Compare a local file against server state; exits non-zero on drift. |

Example: `orion-cli workflows test order-enrichment -f payload.json --trace`

### `channels`

Manages channels. Alias: `ch`.

| Subcommand | Description |
|------------|-------------|
| `list` | List channels. |
| `get <id>` | Show a channel. |
| `create` | Create a channel from JSON. |
| `update <id>` | Replace a channel definition. |
| `delete <id>` | Delete a channel; prompts unless `--yes`. |
| `activate <id>` | Activate a draft channel. |
| `archive <id>` | Archive an active channel. |
| `versions <id>` | List version history. |
| `new-version <id>` | Create a new draft version. |
| `validate` | Validate a definition without creating it. |
| `export` | Export channels as JSON. |
| `import` | Bulk-import from a JSON array file; `--dry-run` previews. |

Example: `orion-cli channels create -f channel.json`

### `connectors`

Manages connectors and their circuit breakers. Alias: `conn`.

| Subcommand | Description |
|------------|-------------|
| `list` | List connectors. |
| `get <id>` | Show a connector. |
| `create` | Create a connector from JSON. |
| `update <id>` | Replace a connector definition. |
| `delete <id>` | Delete a connector; prompts unless `--yes`. |
| `enable <id>` | Enable a disabled connector. |
| `disable <id>` | Disable a connector without deleting it. |
| `test <id>` | Probe the connector's target with the stored config. |
| `validate` | Validate a definition without creating it. |
| `export` | Export connectors as JSON; secrets stay masked. |
| `import` | Bulk-import from a JSON array file; `--dry-run` previews. |
| `circuit-breakers` | List circuit breaker states: `closed`, `open`, or `half_open`. |
| `reset-breaker` | Reset a tripped circuit breaker to closed. |

Example: `orion-cli connectors test payment-api`

### `send`

Sends data to a channel. Synchronous by default; `--async-mode` submits for background processing and returns a trace ID.

| Flag | Description |
|------|-------------|
| `<channel>` | Channel name to send data to. |
| `-f, --file <path>` | JSON payload from a file. |
| `-d, --data <json>` | Inline JSON payload. |
| `--stdin` | Read the payload from stdin. |
| `--async-mode` | Submit for async processing; returns a trace ID. Alias: `--async`. |
| `--wait` | With `--async-mode`, poll until the trace completes. |
| `--timeout <secs>` | Timeout for `--wait`. Default: `60`. |
| `--metadata <json>` | Metadata object attached to the request. |
| `--profile` | Request server-side execution profiling. Sync only. |

Example: `orion-cli send orders -f order.json --async-mode --wait`

### `traces`

Views execution traces.

| Subcommand | Description |
|------------|-------------|
| `list` | List traces; filter with `--status`, `--channel`, and `--mode`. |
| `get <id>` | Show trace details, including the result or error. |
| `wait <id>` | Poll until the trace completes. Exit codes: `0` completed, `1` failed, `2` timeout. |

Example: `orion-cli traces list --status failed --channel orders`

### `engine`

Controls the engine. Alias: `eng`.

| Subcommand | Description |
|------------|-------------|
| `status` | Show engine status: version, uptime, workflow and channel counts. |
| `reload` | Hot-reload the engine from the database. |

Example: `orion-cli engine reload`

### `functions`

`list` shows the workflow task functions registered in the engine and their input schemas. Alias: `fn`. The catalog is documented in [Task Functions](./functions.md).

Example: `orion-cli functions list`

### `metrics`

Fetches `GET /metrics` from the server. Default output is a reformatted list; `--raw` prints the Prometheus exposition text. Series are documented in the [Metrics Reference](./metrics.md).

Example: `orion-cli metrics --raw`

### `audit-logs`

`list` shows audit log entries of admin actions. Alias: `audit`.

Example: `orion-cli audit-logs list`

### `backups`

Creates and lists database backups. SQLite only.

| Subcommand | Description |
|------------|-------------|
| `create` | Create a backup. |
| `list` | List existing backups. |

Example: `orion-cli backups create`

### `packages`

Inspects package promotion receipts. Alias: `pkg`.

| Subcommand | Description |
|------------|-------------|
| `list` | List package receipts. |
| `get <name>` | Show a package's current receipt and version history. |

Example: `orion-cli packages get payments`

### `dlq`

Inspects and drains the trace dead-letter queue.

| Subcommand | Description |
|------------|-------------|
| `list` | List dead-letter entries. |
| `get <id>` | Show one entry, including its payload. |
| `requeue <id>` | Reset an entry's retry counter so the next retry pass picks it up. |
| `purge --older-than-hours <n>` | Permanently delete exhausted entries older than the cut-off. The flag is required. |

Example: `orion-cli dlq purge --older-than-hours 168`

### `benchmark`

Runs a performance benchmark against the server. Alias: `bench`.

| Flag | Description |
|------|-------------|
| `-n, --requests <n>` | Requests per scenario. Default: `100`. |
| `-c, --concurrency <n>` | Concurrent requests. Default: `10`. |
| `--timeout <secs>` | Per-request timeout. Default: `30`. |
| `--scenario <name>` | Built-in scenario to run. Default: `all`. |
| `--workflow <id>` | Benchmark an existing workflow instead of the built-in scenarios. |
| `--channel <name>` | Channel to send to; required with `--workflow`. |
| `-f, --file` / `-d, --data` | Payload for `--workflow`. |
| `--cleanup-only` | Only clean up leftover benchmark resources. |

Example: `orion-cli benchmark -n 500 -c 25`

### `completions`

Generates shell completions for `bash`, `zsh`, `fish`, `powershell`, or `elvish`. Alias: `comp`.

Example: `orion-cli completions zsh > ~/.zfunc/_orion-cli`

### `mcp`

`mcp serve` starts the MCP server for AI clients, exposing the Orion API as MCP tools. The default transport is stdio, for local clients such as Claude Desktop and Cursor. `--http` serves the Streamable HTTP transport at `/mcp` for remote clients.

| Flag | Description |
|------|-------------|
| `--http` | Use HTTP transport instead of stdio. |
| `--bind <addr>` | Bind address for HTTP mode. Default: `0.0.0.0:8081`. |

Example: `orion-cli mcp serve --server http://localhost:8080 --http --bind 0.0.0.0:9090`

## Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `ORION_SERVER_URL` | `orion-cli` | Server URL when `--server` is not given. |
| `ORION_API_KEY` | `orion-cli` | Admin API key when `--api-key` is not given. |
| `ORION_API_KEY_HEADER` | `orion-cli` | Header name carrying the key. |
| `NO_COLOR` | `orion-cli` | Disables colored output, like `--no-color`. |
| `ORION_ADMIN_TOKEN` | `orion-server package` | Bearer token sent to the target instance's admin API. |
| `ORION_SECTION__KEY` | `orion-server` | Overrides any config setting, e.g. `ORION_SERVER__PORT`. See the [Configuration Reference](./configuration.md#how-settings-are-resolved). |

## Related

- [Configuration Reference](./configuration.md) — every server setting and its `ORION_*` override.
- [Admin API](./admin-api.md) — the HTTP endpoints `orion-cli` drives.
- [Packages & Promotion](../topology/packages.md) — the promotion model behind `orion-server package`.
- [OpenAPI](./openapi.md) — the spec `dump-openapi` prints.
