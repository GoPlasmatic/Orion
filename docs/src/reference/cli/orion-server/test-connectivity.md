<!-- description: orion-server test-connectivity probes the configured database, so wrong credentials surface before the server tries to start. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server test-connectivity`

Probes the configured database with a no-op query. Catches wrong credentials before the server tries to start.

## Synopsis

```bash
orion-server [-c <config.toml>] test-connectivity [--wait <duration>]
```

## Options

| Flag | Description |
|------|-------------|
| `--wait <duration>` | Keep retrying until the database accepts connections: `60` (seconds), `30s`, `5m`. Only connection failures are retried, as for [`migrate --wait`](./migrate.md). |

Without `--wait`, the database connection is retried for `storage.connect_retry_secs`, printing a line on stderr for each retry.

## Examples

```bash
orion-server -c config.toml test-connectivity
orion-server -c config.toml test-connectivity --wait 2m
```

## Related

- [Server configuration](../../configuration/index.md): the `storage` section it probes.
- [Troubleshooting](../../../operate/maintain/troubleshooting.md): what to do when the probe fails.
- [Install Orion](../../../get-started/install.md): the first run.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
