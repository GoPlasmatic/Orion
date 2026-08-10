# Run the Examples

The repository ships eight ready-to-deploy services. Each is a **package** — the
channels, workflows, and (when needed) connector that make up one service,
grouped so they deploy and [promote](../concepts/packages.md) together.

Most are self-contained: built-in functions and JSONLogic only, no database and
no connector to set up. Every workflow here is linted and deployed end-to-end in
CI, so what you copy is what CI proves.

## 1. Get the files

No install method produces a checkout, so start with one:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion/examples
```

## 2. Deploy one

With a server on `http://localhost:8080`:

```bash
./deploy.sh high-value-order
```

`deploy.sh` creates and activates every workflow the package ships, then every
channel (creating the connector first, if it has one), then POSTs
`request.json` to the primary channel and prints the response. It needs `curl`
and `python3`. Re-running is safe: objects that already exist are skipped.

A package with no HTTP route — `kafka-order-events` — deploys the same way, and
the script prints the topic it now consumes instead of sending a request.

## The packages

| Package | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/high-value-order) | `POST /high-value-orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-classification) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/iot-sensor-alert) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/webhook-transform) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/notification-routing) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/postgres-orders) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |
| [`channel-composition`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/channel-composition) | `POST /order-enrichment` | **Two services:** one calls the other in-process with `channel_call` |
| [`kafka-order-events`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/kafka-order-events) | topic `orders.events` | **Kafka ingress:** consumes a topic, stamps the record's coordinates |

## What is in a package directory

| File | Sent to | Purpose |
|------|---------|---------|
| `workflow.json` | `POST /api/v1/admin/workflows` | The task pipeline — the logic |
| `workflow-<name>.json` *(optional)* | `POST /api/v1/admin/workflows` | Additional workflows, when the package is more than one service |
| `channel.json` | `POST /api/v1/admin/channels` | The endpoint that routes to the workflow |
| `channel-<name>.json` *(optional)* | `POST /api/v1/admin/channels` | Additional channels |
| `request.json` | `POST /api/v1/data/<route>` | A sample request to try it |
| `connector.json` *(optional)* | `POST /api/v1/admin/connectors` | A named connection to an external system, when the package needs one |

Every entity carries a `tags: ["pkg:<name>"]` label. That label is what marks it
as part of the package, and what package export selects on.

> Requests use the `{ "data": { … } }` envelope. Orion unwraps `data` into the
> workflow payload, which `parse_json` reads with `"source": "payload"`.

## …or deploy it step by step

`deploy.sh` is four API calls and a request. Running them yourself shows the
lifecycle each one drives:

```bash
cd packages/high-value-order

# 1. Create the workflow — it lands as a draft
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

> [!TIP]
> **Test before activating.** A draft workflow can be dry-run against sample
> data without serving any traffic. `/test` takes the same `{ "data": … }`
> envelope and returns an execution trace showing which tasks ran or were
> skipped:
>
> ```bash
> curl -X POST http://localhost:8080/api/v1/admin/workflows/high-value-order/test \
>   -H 'Content-Type: application/json' --data @request.json
> ```

## The offline test suite

[`examples/workflow-tests/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/workflow-tests)
holds `*.case.json` regression cases for the self-contained packages. Each runs
the real workflow JSON through the real engine with no server, database, or
network:

```bash
orion-server test examples/workflow-tests
```

It exits non-zero on any failure, so it gates CI alongside `orion-server lint`.
[`examples/use-cases/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/use-cases)
does the same job against a **real server**: the repo's e2e suite deploys each
case through `orion-cli` and asserts the live responses.

## Next steps

- [Test & Promote a Service](./test-and-promote.md) — take one of these
  packages from a local run to a second instance.
- [Your First Connector](./first-connector.md) — build `postgres-orders` by
  hand, one step at a time.
- [Workflow Schema](../reference/workflows.md) and
  [Task Functions](../reference/functions.md) — the reference behind every file
  above.
