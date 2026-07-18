# Orion Examples

Ready-to-deploy [channels and workflows](../docs/src/reference/workflows.md) you
can POST to a running Orion instance and call immediately.

Most examples are **self-contained and zero-dependency** — they use only the
built-in data functions (`parse_json`, `map`, and JSONLogic conditions), so they
run against a fresh `orion-server` with no database, connectors, or external
services to set up. The exception is [`postgres-orders`](postgres-orders/),
which ships a `docker compose` file and shows the connector-backed side of
Orion: `data_query`/`data_write` against a real PostgreSQL database. Every
workflow is linted and deployed end-to-end in CI.

New to Orion? `./quickstart.sh` deploys your first service (workflow + channel,
activated, first request sent) against a running instance in one command.

## Layout

Each example directory holds three request bodies:

| File | Sent to | Purpose |
|------|---------|---------|
| `workflow.json` | `POST /api/v1/admin/workflows` | The task pipeline (the logic) |
| `channel.json`  | `POST /api/v1/admin/channels`  | The endpoint that routes to the workflow |
| `request.json`  | `POST /api/v1/data/<route>`    | A sample request to try it |
| `connector.json` *(optional)* | `POST /api/v1/admin/connectors` | A named connection to an external system, when the example needs one |

> Requests use the `{ "data": { … } }` envelope. Orion unwraps `data` into the
> workflow payload, which `parse_json` reads via `"source": "payload"`.

## Run an example

Start Orion (`orion-server`, listening on `http://localhost:8080`), then deploy
any example in one command:

```bash
./deploy.sh high-value-order
```

`deploy.sh` creates and activates the workflow, creates and activates the
channel (and creates the connector first, if the example has one), then POSTs
`request.json` and prints the response. It needs `curl` and `python3`.
Re-running is safe — objects that already exist are skipped.

### …or step by step

```bash
cd high-value-order

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
curl -X PATCH http://localhost:8080/api/v1/admin/channels/orders/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'

# 5. Send a request
curl -X POST http://localhost:8080/api/v1/data/orders \
  -H 'Content-Type: application/json' --data @request.json
```

> **Tip — test before activating.** Dry-run a draft workflow against sample data
> without serving any traffic. The `/test` endpoint takes the same `{ "data": … }`
> envelope and returns an execution trace showing which tasks ran or were skipped:
> ```bash
> curl -X POST http://localhost:8080/api/v1/admin/workflows/high-value-order/test \
>   -H 'Content-Type: application/json' --data @high-value-order/request.json
> ```

## The examples

| Example | Endpoint | What it shows |
|---------|----------|---------------|
| [`high-value-order`](high-value-order/) | `POST /orders` | Flag orders over a threshold; build an alert string with `cat` |
| [`order-classification`](order-classification/) | `POST /order-tiers` | Tiered classification driven by task-level conditions |
| [`iot-sensor-alert`](iot-sensor-alert/) | `POST /sensors` | Range-based severity with `and` / `or` |
| [`webhook-transform`](webhook-transform/) | `POST /webhooks` | Normalize provider payloads with `var` mapping (null-safe) |
| [`notification-routing`](notification-routing/) | `POST /notifications` | Progressive routing with the `in` set-membership operator |
| [`postgres-orders`](postgres-orders/) | `POST /record-order` | **Connector-backed:** `data_write` insert + `data_query` with relations against PostgreSQL (ships `docker compose`) |

## Beyond these examples

Workflows that talk to external systems — `data_query`/`data_write`,
`http_call`, `db_read`/`db_write`, `cache_*`, `mongo_read`, `publish_kafka` —
need a **connector** (`POST /api/v1/admin/connectors`); `postgres-orders` is
the worked example of that pattern, and `channel_call` composes channels
in-process. See:

- [Function Reference](../docs/src/reference/functions.md) — every function's input schema
- [Workflow Reference](../docs/src/reference/workflows.md) — workflow shape, conditions, and lifecycle
- [Use Cases & Patterns](../docs/src/tutorials/use-cases.md) — connector-backed and composition walkthroughs
- [Orion CLI](https://github.com/GoPlasmatic/Orion-cli) — deploy with `orion-cli workflows create -f workflow.json`
