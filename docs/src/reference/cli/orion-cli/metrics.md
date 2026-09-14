<!-- description: orion-cli metrics fetches GET /metrics from the server as a reformatted list, or the raw Prometheus exposition text with --raw. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli metrics`

Fetches `GET /metrics` from the server. Default output is a reformatted list; `--raw` prints the Prometheus exposition text. Series are documented in the [Metrics Reference](../../metrics.md).

## Synopsis

```bash
orion-cli metrics [--raw]
```

## Examples

```bash
orion-cli metrics --raw
```

## Related

- [Metrics](../../metrics.md): every series, with its labels.
- [Monitor and alert](../../../operate/run/monitoring.md): turning the series into alerts.
- [`orion-cli health`](./health.md): the other operational read.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
