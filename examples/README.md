# Orion Examples

Ready-to-deploy example **packages** you can POST to a running Orion instance
and call immediately. A package is Orion's unit of shipping: the
[channel, workflow, and connector](https://goplasmatic.github.io/Orion/architecture/overview.html#three-primitives)
that belong to one service, grouped so they deploy — and later
[export, promote, and version](https://goplasmatic.github.io/Orion/topology/packages.html)
— together. One Orion instance runs many packages side by side: a modular
monolith, where each package stays independently deployable without becoming
its own service to operate.

Most packages here are **self-contained and zero-dependency** — they use only
the built-in data functions (`parse_json`, `map`, and JSONLogic conditions), so
they run against a fresh `orion-server` with no database, connectors, or
external services to set up. The exception is
[`postgres-orders`](packages/postgres-orders/), which ships a `docker compose`
file and shows the connector-backed side of Orion: `data_query`/`data_write`
against a real PostgreSQL database. Every workflow is linted and deployed
end-to-end in CI.

New to Orion? `./quickstart.sh` deploys your first service (workflow + channel,
activated, first request sent) against a running instance in one command.

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

Each package directory holds a set of request bodies:

| File | Sent to | Purpose |
|------|---------|---------|
| `workflow.json` | `POST /api/v1/admin/workflows` | The task pipeline (the logic) |
| `channel.json`  | `POST /api/v1/admin/channels`  | The endpoint that routes to the workflow |
| `request.json`  | `POST /api/v1/data/<route>`    | A sample request to try it |
| `connector.json` *(optional)* | `POST /api/v1/admin/connectors` | A named connection to an external system, when the package needs one |

Every entity carries a `tags: ["pkg:<name>"]` label — that is what marks it as
belonging to the package, and what the package export selects on (see
[below](#from-deployed-example-to-package-artifact)).

> Requests use the `{ "data": { … } }` envelope. Orion unwraps `data` into the
> workflow payload, which `parse_json` reads via `"source": "payload"`.

## Run an example

Start Orion (`orion-server`, listening on `http://localhost:8080`), then deploy
any package in one command:

```bash
./deploy.sh high-value-order
```

`deploy.sh` creates and activates the workflow, creates and activates the
channel (and creates the connector first, if the package has one), then POSTs
`request.json` and prints the response. It needs `curl` and `python3`.
Re-running is safe — objects that already exist are skipped.

### …or step by step

```bash
cd packages/high-value-order

# 1. Create the workflow (saved as a draft)
curl -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' --data @workflow.json

# 2. Activate it
curl -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'

# 3. Create the channel
curl -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' --data @channel.json

# 4. Activate it
curl -X PATCH http://localhost:8080/api/v1/admin/channels/high-value-orders/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'

# 5. Send a request
curl -X POST http://localhost:8080/api/v1/data/high-value-orders \
  -H 'Content-Type: application/json' --data @request.json
```

> **Tip — test before activating.** Dry-run a draft workflow against sample data
> without serving any traffic. The `/test` endpoint takes the same `{ "data": … }`
> envelope and returns an execution trace showing which tasks ran or were skipped:
> ```bash
> curl -X POST http://localhost:8080/api/v1/admin/workflows/high-value-order/test \
>   -H 'Content-Type: application/json' --data @request.json
> ```

## The packages

| Package | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](packages/high-value-order/) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](packages/order-classification/) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](packages/iot-sensor-alert/) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](packages/webhook-transform/) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](packages/notification-routing/) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](packages/postgres-orders/) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |

## From deployed example to package artifact

The per-file layout above is a package in **source form** — readable JSON you
can review and edit. Once deployed, the `pkg:<name>` tags let
`orion-server package export` capture the same service as a single versioned
**artifact** and promote it to another instance:

```bash
# Capture the deployed example as a package artifact
orion-server package export -s http://localhost:8080 \
  --tag pkg:high-value-order --name high-value-order --version 1.0.0 \
  -o high-value-order-1.0.0.json

# Validate it offline, then apply it to a second instance
orion-server package lint  -f high-value-order-1.0.0.json
orion-server package apply -s http://localhost:9090 -f high-value-order-1.0.0.json
```

`export` pulls the tagged channels, their workflows, and every connector those
workflows reference; `apply` stages, activates in dependency order, and records
a version receipt on the target. See
[Packages & Promotion](https://goplasmatic.github.io/Orion/topology/packages.html)
for the full flow (lint, plan, diff, rollback, secrets).

## Offline regression tests

[`workflow-tests/`](workflow-tests/) holds `*.case.json` regression cases for
the self-contained packages above — each runs the real workflow JSON through
the real engine with no server, database, or network:

```bash
orion-server test examples/workflow-tests
```

The runner exits non-zero on any failure, so it gates CI alongside
`orion-server lint`. See [its README](workflow-tests/README.md) for the case
format (including connector stubs).

## Server-backed use cases

[`use-cases/`](use-cases/) holds end-to-end scenario definitions for the
packages above: each references a package's `workflow.json` (never a copy)
and pairs it with the requests to send and the responses to expect. Unlike
`workflow-tests/`, these run against a **real server**: the repo's e2e suite
(`just e2e`, from `tests/e2e/`) deploys each case through the `orion-cli`
binary and asserts the live responses — so CI proves the shipped examples
against real traffic, not just offline. See
[its README](use-cases/README.md) for the case format and how to add one.

## Beyond these examples

Workflows that talk to external systems — `data_query`/`data_write`,
`http_call`, `db_read`/`db_write`, `cache_*`, `mongo_read`, `publish_kafka` —
need a **connector** (`POST /api/v1/admin/connectors`); `postgres-orders` is
the worked example of that pattern, and `channel_call` composes channels
in-process. See:

- [Function Reference](https://goplasmatic.github.io/Orion/reference/functions.html) — every function's input schema
- [Workflow Reference](https://goplasmatic.github.io/Orion/reference/workflows.html) — workflow shape, conditions, and lifecycle
- [Packages & Promotion](https://goplasmatic.github.io/Orion/topology/packages.html) — shipping packages between instances
- [Use Cases & Patterns](https://goplasmatic.github.io/Orion/tutorials/use-cases.html) — connector-backed and composition walkthroughs
- [Orion CLI](../crates/orion-cli/) — deploy with `orion-cli workflows create -f workflow.json`
