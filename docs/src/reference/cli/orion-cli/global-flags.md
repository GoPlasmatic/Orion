<!-- description: The flags every orion-cli command accepts, the environment variables and config file behind them, and the paging and sorting controls shared by every list. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Global flags

Every `orion-cli` command accepts the same global flags. Settings resolve in precedence order: CLI flags, then environment variables, then `~/.orion/config.toml`.

## Synopsis

```bash
orion-cli [global flags] <command> [subcommand] [args] [flags]
```

## Options

| Flag | Description |
|------|-------------|
| `--server <url>` | Orion server URL. Overrides the config file and `ORION_SERVER_URL`. |
| `--api-key <key>` | API key for admin authentication. Falls back to `ORION_API_KEY`, then the `api_key` in `~/.orion/config.toml`. When the key would travel over plain `http://` to any host but the local machine, a warning is printed to stderr — use `https://` for a remote server. |
| `--api-key-file <path>` | Read the admin API key from `<path>` (or `-` for standard input), avoiding exposing the secret on the process command line. Conflicts with `--api-key`. |
| `--api-key-header <name>` | Header name carrying the key. Default: `Authorization` with a `Bearer` prefix. |
| `--change-context <ctx>` | Audit label for this change, for example `ticket=OPS-4412`. Sent as `X-Orion-Change-Context` and recorded under `details.change_context` on every audit row the command writes. Also read from `ORION_CHANGE_CONTEXT`. |
| `--output <format>` | Output format: `table` (default), `json`, or `yaml`. |
| `--quiet` | Print only IDs or minimal info. |
| `--verbose` | Show full response bodies and extra details. |
| `--no-color` | Disable colored output. |
| `--yes` | Skip confirmation prompts. |

### Paging

Every `list` pages: 50 rows by default, 1000 at most. When more rows follow, the line under the table names the flag that fetches them (`next page: --cursor …`). A truncated listing never reads as a complete one.

| Flag | Applies to | Description |
|------|-----------|-------------|
| `--limit <n>` | every `list` | Page size, clamped to 1–1000. |
| `--cursor <c>` | as above | Resume after this position: the previous page's `next_cursor`, unmodified. |
| `--include-total` | as above | Also count the whole filtered set. Off by default: the count is a full scan. |

Lists are newest-first and have no `--offset` or `--sort-by`; filters narrow them.

## Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `ORION_SERVER_URL` | `orion-cli` | Server URL when `--server` is not given. |
| `ORION_API_KEY` | `orion-cli` | Admin API key when `--api-key` is not given. |
| `ORION_API_KEY_HEADER` | `orion-cli` | Header name carrying the key. |
| `ORION_CHANGE_CONTEXT` | `orion-cli` | Audit change context when `--change-context` is not given. |
| `NO_COLOR` | `orion-cli` | Disables colored output, like `--no-color`. |

`ORION_SECTION__KEY` variables belong to the server, not the client; see [Environment variables](../../environment-variables.md).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Command succeeded, or a polled deployment or trace completed. |
| `1` | Runtime, network, server, or execution error, or lint and clippy findings. |
| `2` | Usage error, invalid CLI arguments, or preflight and gate refusal. |
| `3` | Polling timeout (`deployments wait`, `deployments apply --wait-ms`, `traces wait`, `send --wait`). |

## Related

- [`orion-cli config`](./config.md): the settings file behind the flags.
- [Admin API › Authentication](../../admin-api/authentication.md): what the key and header mean on the wire.
- [Audit logs](../../../operate/run/audit-logs.md): where `--change-context` lands.
- [`orion-cli` commands](./index.md): every subcommand.
