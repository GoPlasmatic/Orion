# Orion Examples

Ready-to-deploy example **packages** you can POST to a running Orion instance
and call immediately. A package is Orion's unit of shipping: the channel,
workflow, and connector that belong to one service, grouped so they deploy — and
later export, promote, and version — together.

**The walkthrough lives in the documentation:**
[Run the Examples](https://goplasmatic.github.io/Orion/getting-started/examples.html)
— what each package shows, the step-by-step deploy, and how to dry-run a draft
before activating it.

## Layout

```
examples/
├── packages/           # each directory = one deployable package
│   └── <name>/
├── workflow-tests/     # offline *.case.json regression suite (references packages/)
├── use-cases/          # live e2e scenarios for the packages (run by tests/e2e)
├── deploy.sh           # deploy one package end-to-end
└── quickstart.sh       # your first service in one command
```

## Quick start

Start Orion (`orion-server`, listening on `http://localhost:8080`), then:

```bash
./deploy.sh high-value-order          # deploy one package and send its sample request
./quickstart.sh                       # or: your first service, built from scratch
```

## The packages

| Package | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](packages/high-value-order/) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](packages/order-classification/) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](packages/iot-sensor-alert/) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](packages/webhook-transform/) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](packages/notification-routing/) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](packages/postgres-orders/) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |

Every entity carries a `tags: ["pkg:<name>"]` label — that is what marks it as
belonging to the package, and what `orion-server package export` selects on.

## Testing

```bash
orion-server test examples/workflow-tests   # offline: real engine, no server or network
just e2e                                    # live: tests/e2e drives a real server via orion-cli
```

[`workflow-tests/README.md`](workflow-tests/README.md) documents the case format
(including connector stubs); [`use-cases/README.md`](use-cases/README.md)
documents the server-backed scenario format.

## Documentation

- [Run the Examples](https://goplasmatic.github.io/Orion/getting-started/examples.html) — the full walkthrough
- [Test & Promote a Service](https://goplasmatic.github.io/Orion/getting-started/test-and-promote.html) — from a local run to a second instance
- [Packages](https://goplasmatic.github.io/Orion/concepts/packages.html) — what a package is and why the boundary sits there
- [Task Functions](https://goplasmatic.github.io/Orion/reference/functions.html) — every function's input schema
- [Workflow Schema](https://goplasmatic.github.io/Orion/reference/workflows.html) — workflow shape, conditions, and lifecycle
- [Orion CLI](../crates/orion-cli/) — deploy with `orion-cli workflows create -f workflow.json`
