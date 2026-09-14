<!-- description: orion-cli health checks the server's health, version and component status, and exits 1 when any component is degraded, for scripts and probes. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli health`

Checks server health, version, and component status. Exits `1` when any component is degraded.

## Synopsis

```bash
orion-cli health
```

## Examples

```bash
orion-cli health
```

## Related

- [Monitor and alert](../../../operate/run/monitoring.md): the health endpoints and what degraded means.
- [Admin API › Operational endpoints](../../data-api.md#operational-endpoints): the routes behind the command.
- [Troubleshooting](../../../operate/maintain/troubleshooting.md): degraded but serving.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
