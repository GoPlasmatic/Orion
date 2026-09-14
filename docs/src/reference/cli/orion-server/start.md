<!-- description: Starting orion-server: the -c config flag every subcommand shares, how defaults, the file and ORION_* overrides merge, and what a bare invocation does. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Start the server

Running `orion-server` with no subcommand starts the server. The global flag `-c, --config <path>` names the TOML config file and applies to every subcommand.

## Synopsis

```bash
orion-server [-c <config.toml>]
```

## Description

Each subcommand loads the same merged configuration: defaults, then the file, then `ORION_*` environment overrides. See [How settings are resolved](../../configuration/how-settings-are-resolved.md). Both binaries accept `--version`, which prints the version, git hash, and build timestamp.

## Options

| Flag | Description |
|------|-------------|
| `-c, --config <path>` | The TOML configuration file. Applies to every subcommand. |
| `--version` | Print the version, git hash and build timestamp, then exit. |

## Examples

```bash
orion-server                          # Start with defaults
orion-server -c config.toml           # Start with a config file
```

## Related

- [Server configuration](../../configuration/index.md): every setting, its default and its `ORION_*` override.
- [Install Orion](../../../get-started/install.md): the first run, on every platform.
- [Production checklist](../../../operate/production-checklist.md): what to set before the server faces traffic.
- [`orion-server` commands](./index.md): every subcommand.
