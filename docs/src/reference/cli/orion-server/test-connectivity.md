<!-- description: orion-server test-connectivity probes the configured database, and Kafka when enabled, so wrong credentials surface before the server tries to start. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server test-connectivity`

Probes the configured database with a no-op query, and Kafka when `kafka.enabled = true`. Catches wrong credentials before the server tries to start.

## Synopsis

```bash
orion-server [-c <config.toml>] test-connectivity
```

## Examples

```bash
orion-server -c config.toml test-connectivity
```

## Related

- [Server configuration](../../configuration/index.md): the `storage` and `kafka` sections it probes.
- [Troubleshooting](../../../operate/maintain/troubleshooting.md): what to do when the probe fails.
- [Install Orion](../../../get-started/install.md): the first run.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
