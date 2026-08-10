# Examples

The repository ships ready-to-deploy example **packages** under
[`examples/packages/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages)
— JSON you POST to a running instance and call immediately. Each directory is
one package: the channel, workflow, and (when needed) connector that make up a
single service, grouped so they deploy and
[promote](../topology/packages.md) together. Most are **self-contained and
zero-dependency**: only built-in functions (`parse_json`, `map`, JSONLogic
conditions), no database or connectors to set up. Every workflow is linted and
deployed end-to-end in CI.

With a server on `http://localhost:8080`, deploy any of them in one command
from the repository root:

```bash
./examples/deploy.sh high-value-order
```

| Package | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/high-value-order) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-classification) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/iot-sensor-alert) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/webhook-transform) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/notification-routing) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/postgres-orders) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |

Each directory holds `workflow.json` (the logic), `channel.json` (the
endpoint) and `request.json` (a sample call), every entity tagged
`pkg:<name>` so the deployed example can be exported as a versioned package
artifact and applied to another instance — see
[Packages & Promotion](../topology/packages.md).
[`examples/README.md`](https://github.com/GoPlasmatic/Orion/blob/main/examples/README.md)
documents the layout, a step-by-step curl walkthrough, how to dry-run a draft
workflow before activating it, and the export flow.

[`examples/workflow-tests/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/workflow-tests)
holds offline `*.case.json` regression cases for the self-contained packages —
`orion-server test examples/workflow-tests` runs the real workflow JSON
through the real engine with no server or network, and exits non-zero on any
failure, so it gates CI alongside `orion-server lint`.

New to Orion? [`examples/quickstart.sh`](https://github.com/GoPlasmatic/Orion/blob/main/examples/quickstart.sh)
deploys your first service (workflow + channel, activated, first request sent)
against a running instance in one command — the same flow the
[Install & First Service](../getting-started/install.md) tutorial walks through.
