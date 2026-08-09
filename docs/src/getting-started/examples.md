# Examples

The repository ships ready-to-deploy examples under
[`examples/`](https://github.com/GoPlasmatic/Orion/tree/main/examples) — JSON
you POST to a running instance and call immediately. Most are
**self-contained and zero-dependency**: only built-in functions
(`parse_json`, `map`, JSONLogic conditions), no database or connectors to set
up. Every workflow is linted and deployed end-to-end in CI.

With a server on `http://localhost:8080`, deploy any of them in one command
from the repository root:

```bash
./examples/deploy.sh high-value-order
```

| Example | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](https://github.com/GoPlasmatic/Orion/tree/main/examples/high-value-order) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](https://github.com/GoPlasmatic/Orion/tree/main/examples/order-classification) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](https://github.com/GoPlasmatic/Orion/tree/main/examples/iot-sensor-alert) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](https://github.com/GoPlasmatic/Orion/tree/main/examples/webhook-transform) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](https://github.com/GoPlasmatic/Orion/tree/main/examples/notification-routing) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](https://github.com/GoPlasmatic/Orion/tree/main/examples/postgres-orders) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |

Each directory holds `workflow.json` (the logic), `channel.json` (the
endpoint) and `request.json` (a sample call);
[`examples/README.md`](https://github.com/GoPlasmatic/Orion/blob/main/examples/README.md)
documents the layout, a step-by-step curl walkthrough, and how to dry-run a
draft workflow before activating it.

[`examples/workflow-tests/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/workflow-tests)
holds offline `*.case.json` regression cases for the self-contained examples —
`orion-server test examples/workflow-tests` runs the real workflow JSON
through the real engine with no server or network, and exits non-zero on any
failure, so it gates CI alongside `orion-server lint`.

New to Orion? [`examples/quickstart.sh`](https://github.com/GoPlasmatic/Orion/blob/main/examples/quickstart.sh)
deploys your first service (workflow + channel, activated, first request sent)
against a running instance in one command — the same flow the
[Install & First Service](../tutorials/cli-setup.md) tutorial walks through.
