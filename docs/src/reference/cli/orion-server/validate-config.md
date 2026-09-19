<!-- description: orion-server validate-config checks the merged configuration without starting and prints it with secrets masked, as TOML, JSON or a short summary. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server validate-config`

Validates the configuration without starting the server, then prints the full effective config with secrets masked. Exits non-zero on an invalid value.

## Synopsis

```bash
orion-server validate-config [--format <toml|json|summary>]
```

## Options

| Flag | Description |
|------|-------------|
| `--format` | Output format: `toml` (default), `json`, or `summary` (a short human summary). |

## Examples

```bash
orion-server -c config.toml validate-config --format summary
```

A placeholder that cannot be resolved fails the command with the file, line and column. The line below is a `${VAR:?message}` whose variable is unset:

```console
$ orion-server -c config.toml validate-config
Error: Configuration error: ORION_STATE_DB_URL is required: set it to the state database (config.toml:12:7)
```

## Related

- [Server configuration](../../configuration/index.md): every setting, its default and its `ORION_*` override.
- [Environment variables](../../environment-variables.md): the five ways the process environment reaches a setting.
- [Install Orion](../../../get-started/install.md): the first run, with a config file.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
