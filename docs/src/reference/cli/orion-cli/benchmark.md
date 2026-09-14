<!-- description: orion-cli benchmark runs the built-in load scenarios, or an existing workflow through a channel, with request count, concurrency and timeout flags. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli benchmark`

Runs a performance benchmark against the server. Alias: `bench`.

## Synopsis

```bash
orion-cli benchmark [flags]
```

## Options

| Flag | Description |
|------|-------------|
| `-n, --requests <n>` | Requests per scenario. Default: `100`. |
| `-c, --concurrency <n>` | Concurrent requests. Default: `10`. |
| `--timeout <secs>` | Per-request timeout. Default: `30`. |
| `--scenario <name>` | Built-in scenario to run. Default: `all`. |
| `--workflow <id>` | Benchmark an existing workflow instead of the built-in scenarios. |
| `--channel <name>` | Channel to send to; required with `--workflow`. |
| `-f, --file` / `-d, --data` | Payload for `--workflow`. |
| `--cleanup-only` | Only clean up leftover benchmark resources. |

## Examples

```bash
orion-cli benchmark -n 500 -c 25
```

## Related

- [Benchmark suite](https://github.com/GoPlasmatic/Orion/tree/main/crates/orion-server/tests/benchmark): the scenarios, and the published numbers per release.
- [Monitor and alert](../../../operate/run/monitoring.md): watching a run from the server side.
- [`orion-cli send`](./send.md): one request, by hand.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
