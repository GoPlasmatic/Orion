# Orion Examples

Ready-to-deploy example **packages** you can deploy to a running Orion instance
and call immediately. A package here is one directory holding the channel,
workflow, and connector that belong to one service. Each directory is a
definition set: `orion-cli compile` turns it into a deployment.

**The walkthrough lives in the documentation:**
[Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html)
shows what each package does and how to deploy it.

## Layout

```
examples/
├── packages/           # each directory = one deployable package
│   └── <name>/
├── plugins/            # plugin source: a Rust crate on orion-plugin-sdk + build.sh
├── workflow-tests/     # offline *.case.json regression suite (references packages/)
├── use-cases/          # live e2e scenarios for the packages (run by tests/e2e)
├── deploy.sh           # compile + deploy one or more packages, send their requests
└── quickstart.sh       # your first service in one command
```

## Quick start

Start Orion (`orion-server`, listening on `http://localhost:8080`), then compile
one package and deploy it:

```bash
orion-cli compile examples/packages/high-value-order -o dist/high-value-order
orion-cli deployments apply -d dist/high-value-order
curl -s -X POST http://localhost:8080/api/v2/data/high-value-orders \
  -H 'Content-Type: application/json' --data @examples/packages/high-value-order/request.json
```

A deployment states everything a node serves. Applying a second package's
directory replaces the first. To run several side by side, compile them as
one set — `deploy.sh` does that, then sends each package's `request.json`:

```bash
./examples/deploy.sh high-value-order                       # one package
./examples/deploy.sh order-classification webhook-transform # both, as one deployment
```

It finds `orion-cli` under `target/` (build it with `cargo build -p orion-cli`),
on `PATH`, or through `ORION_CLI`. `quickstart.sh` deploys a one-workflow
service the same way. Both scripts need a running Orion server.

## The packages

| Package | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](packages/high-value-order/) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](packages/order-classification/) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](packages/iot-sensor-alert/) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`fixed-width-statement`](packages/fixed-width-statement/) | `POST /statements` | **Plugin-backed:** a fixed-width codec compiled to WebAssembly ([source](plugins/fixed-width/)) decodes the line — needs `plugins.enabled = true` |
| [`webhook-transform`](packages/webhook-transform/) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](packages/notification-routing/) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](packages/postgres-orders/) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |
| [`channel-composition`](packages/channel-composition/) | `POST /order-enrichment` | **Two services:** one calls the other in-process with `channel_call` |
| [`order-summary`](packages/order-summary/) | `POST /order-summary` | The dependency-free files behind the documentation's HTTP-flow tutorial |
| [`nightly-rollup`](packages/nightly-rollup/) | `0 15 2 * * *` (no route) | **Scheduled:** a `protocol: "cron"` channel runs the workflow on a six-field expression; every instant becomes a durable occurrence |
| [`c4-referee`](packages/c4-referee/) | `POST /c4/move` | **Plugin-backed:** the rules of Connect Four compiled to WebAssembly ([source](plugins/c4-rules/)) play the column the caller names and settle a win, a draw or a forfeit — needs `plugins.enabled = true` |

## Testing

```bash
orion-cli test examples/workflow-tests \
  --plugin-dir examples/packages/fixed-width-statement \
  --plugin-dir examples/packages/c4-referee            # offline: real engine, no server or network
just e2e                                                # live: tests/e2e drives a real server via orion-cli
```

The two `--plugin-dir`s hand the runner the plugins the cases call for
real; without them those cases fail rather than pass on a stub.

[`workflow-tests/README.md`](workflow-tests/README.md) documents the case format
(including connector stubs); [`use-cases/README.md`](use-cases/README.md)
documents the server-backed scenario format.

## Documentation

- [Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html) — the full walkthrough
- [Deployment endpoints](https://docs.goplasmatic.io/reference/admin-api/deployments.html) — how a compiled directory is deployed
- [Task Functions](https://docs.goplasmatic.io/reference/functions/index.html) — every function's input schema
- [Workflow Schema](https://docs.goplasmatic.io/reference/workflows.html) — workflow shape, conditions, and matching
- [Orion CLI](../crates/orion-cli/) — deploy with `orion-cli deployments apply -d <dir>`
