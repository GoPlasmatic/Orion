<!-- description: orion-server test-connectivity probes the configured database, and Kafka when enabled, so wrong credentials surface before the server tries to start. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server test-connectivity`

Probes the configured database with a no-op query, and Kafka when `kafka.enabled = true`. Catches wrong credentials before the server tries to start.

## Synopsis

```bash
orion-server [-c <config.toml>] test-connectivity [--wait <duration>]
```

## Options

| Flag | Description |
|------|-------------|
| `--wait <duration>` | Keep retrying until the database, and Kafka when enabled, accept connections: `60` (seconds), `30s`, `5m`. The duration bounds the whole command, not each dependency. Only connection failures are retried, as for [`migrate --wait`](./migrate.md). |

Without `--wait`, the database connection is retried for `storage.connect_retry_secs`, printing a line on stderr for each retry, and Kafka is probed once.

## Examples

```bash
orion-server -c config.toml test-connectivity
orion-server -c config.toml test-connectivity --wait 2m
```

## Related

- [Server configuration](../../configuration/index.md): the `storage` and `kafka` sections it probes.
- [Troubleshooting](../../../operate/maintain/troubleshooting.md): what to do when the probe fails.
- [Install Orion](../../../get-started/install.md): the first run.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
