# HTTP deployment benchmarks

Run from any directory with `hey`, `jq`, `curl` and `python3` on `PATH`:

```bash
BENCH_DURATION=2s BENCH_CONCURRENCY=4 \
  bash crates/orion-server/tests/benchmark/bench.sh
```

The runner builds matching server and CLI binaries and starts a temporary SQLite
node with plugins enabled. It compiles definition sets and applies complete v2
deployments. It stops its server and removes its temporary state on exit.
Use `ORION_BIN` and `ORION_CLI` to select candidate binaries and
`BENCH_SKIP_BUILD=1` to skip the build. `BENCH_RELEASE=1` selects release binaries;
build both from the same commit before a dedicated performance session.

| Scenario | Workload |
|---|---|
| `baseline` | Health endpoint |
| `simple` | One debug log task, below the configured log level |
| `complex` | Parse the request, classify its amount, enrich the result; output probe requires `vip-US` |
| `multi` | Twelve workflows and twelve channels in one deployment, each running the simple workload; probe all routes, load the first |
| `concurrency` | Simple workload at 1, 10, 50 and 100 clients |
| `reload` | Simple workload while alternating two workflow definitions; require new committed head revisions and convergence |
| `plugin` | Parse + Wasm identity versus parse + JSONLogic echo; probe the echoed input |
| `cluster` | Simple workload through an existing disposable v2 HA stack's load balancer; opt-in |

With no arguments, the seven local scenarios emit eleven result rows. Name one
or more scenarios to select them. `BENCH_DURATION` defaults to `10s` and
`BENCH_CONCURRENCY` to `50`; the concurrency scenario uses its four fixed client
counts. Replacement needs a duration long enough to commit at least one change.
An unchanged directory applies nothing in v2, so `reload` changes the workflow
name between two precompiled sets. Each apply must advance the head. Output
probes precede timed traffic. HTTP error and transport-failure counts make the
runner exit nonzero after printing its results; setup/apply failures stop it.

Set `BENCH_OUTPUT_DIR` to retain raw `hey` output, session metadata and the
replacement count. Keep debug smoke runs separate from published performance
figures. The v2 complex, estate and replacement workloads differ from historical
activation-era scenarios; old numbers do not establish a v2 regression or speedup.
See [the release procedure](../../../../RELEASING.md) for dedicated measurements.

## Cluster scenario

Start the disposable HA topology with a v2 image and every upstream healthy.
Until a published v2 image exists, add `docker-compose.ha.build.yml` to build
this checkout. Supply its admin credential through `ORION_ADMIN_API_KEYS`:

```bash
BENCH_CLUSTER_URL=http://localhost:8080 BENCH_DURATION=30s \
  bash crates/orion-server/tests/benchmark/bench.sh cluster
```

The runner replaces the whole head while benchmarking. On exit it restores the
prior head through compare-and-swap, or commits an empty manifest if no head
existed. If another writer changes the head, it refuses to overwrite that change
and exits with an error. Run on a stack reserved for the benchmark. Repeated
successful probes through the LB precede timing; confirm all replicas healthy
outside the runner too, since the LB can hide an unavailable upstream.

The [run-view benchmark](RUN_VIEW.md) measures engine allocations, execution time
and process RSS through the plugin host without HTTP. It complements this suite.
