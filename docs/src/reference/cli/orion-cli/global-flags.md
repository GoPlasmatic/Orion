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
| `--api-key-header <name>` | Header name carrying the key. Default: `Authorization` with a `Bearer` prefix. |
| `--change-context <ctx>` | Audit label for this change, for example `ticket=OPS-4412`. Sent as `X-Orion-Change-Context` and recorded under `details.change_context` on every audit row the command writes. Also read from `ORION_CHANGE_CONTEXT`. |
| `--output <format>` | Output format: `table` (default), `json`, or `yaml`. |
| `--quiet` | Print only IDs or minimal info. |
| `--verbose` | Show full response bodies and extra details. |
| `--no-color` | Disable colored output. |
| `--yes` | Skip confirmation prompts. |

### Paging and sorting

Every `list` pages: 50 rows by default, 1000 at most. The count under the table
says `Showing 50 of 3120 …` when the page is short of the total, so a truncated
listing never reads as a complete one.

| Flag | Applies to | Description |
|------|-----------|-------------|
| `--limit <n>` | every `list`, and `workflows`/`channels versions` | Page size, clamped to 1–1000. |
| `--offset <n>` | as above | Rows to skip. |
| `--sort-by <col>` | `workflows`, `channels`, `connectors`, `traces` | Column to order by. The accepted columns differ per resource; see each command below. |
| `--sort-order <dir>` | as above | `asc` or `desc`. Defaults to `desc` for the versioned lists (ordered by `priority`) and `asc` for connectors (ordered by `name`). |

## Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `ORION_SERVER_URL` | `orion-cli` | Server URL when `--server` is not given. |
| `ORION_API_KEY` | `orion-cli` | Admin API key when `--api-key` is not given. |
| `ORION_API_KEY_HEADER` | `orion-cli` | Header name carrying the key. |
| `ORION_CHANGE_CONTEXT` | `orion-cli` | Audit change context when `--change-context` is not given. |
| `NO_COLOR` | `orion-cli` | Disables colored output, like `--no-color`. |

`ORION_SECTION__KEY` variables belong to the server, not the client; see [Environment variables](../../environment-variables.md).

## Related

- [`orion-cli config`](./config.md): the settings file behind the flags.
- [Admin API › Authentication](../../admin-api/authentication.md): what the key and header mean on the wire.
- [Audit logs](../../../operate/run/audit-logs.md): where `--change-context` lands.
- [`orion-cli` commands](./index.md): every subcommand.
