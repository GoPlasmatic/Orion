<!-- description: orion-cli config reads and writes the CLI's own settings in ~/.orion/config.toml: the server URL, the API key and header, and the default output format. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli config`

Manages the CLI's own settings in `~/.orion/config.toml`.

## Synopsis

```bash
orion-cli config <set-server|show|get|set> [args]
```

## Description

A stored `api_key` is used by every command, but the flag and
`ORION_API_KEY` both win over it. The file is plain TOML in your home
directory — on a shared machine, prefer the environment variable.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `set-server <url>` | Set the Orion server URL. |
| `show` | Show the current CLI configuration. |
| `get <key>` | Print a single value, for scripting. |
| `set <key> <value>` | Set a value: `server_url`, `default_output`, `api_key`, or `api_key_header`. |

## Examples

```bash
orion-cli config set-server http://localhost:8080
```

## Related

- [Global flags](./global-flags.md): the precedence between flags, environment and this file.
- [Install Orion](../../../get-started/install.md): installing the CLI.
- [Build your first service](../../../get-started/tutorials/first-service.md): the CLI in use, end to end.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
