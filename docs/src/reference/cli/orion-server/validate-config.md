<!-- description: orion-server validate-config checks the merged configuration without starting and prints it with secrets masked, as TOML, JSON or a short summary. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

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

## Related

- [Server configuration](../../configuration/index.md): every setting, its default and its `ORION_*` override.
- [Environment variables](../../environment-variables.md): the five ways the process environment reaches a setting.
- [Install Orion](../../../get-started/install.md): the first run, with a config file.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
