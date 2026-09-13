<!-- description: Ready-to-deploy Orion example packages — order classification, webhook transforms, Kafka events, IoT alerts — each with a step-by-step walkthrough. -->
# Run the Examples

**Tested with:** Orion 1.8.0 · **Last reviewed:** 2026-09-04

The current repository ships ready-to-deploy services. Each is a **package**: the
channels, workflows, and (when needed) connector that make up one service,
grouped so they deploy and [promote](../concepts/packages.md) together.

Most are self-contained: built-in functions and JSONLogic only, no database and
no connector to set up. Every workflow here is linted and deployed end-to-end in
CI, so what you copy is what CI proves.

## Before you start

You need Git, `curl`, Python 3, a POSIX-compatible shell, and an Orion server
running on `http://localhost:8080`. Some packages also require Docker with
Compose; their descriptions call this out. Windows users should run the shell
scripts from WSL.

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
the script prints the topic it now consumes instead of sending a request. It is
one of two examples that are **not** zero-dependency: it needs a broker and a
server started with `[kafka] enabled = true`. Without those the channel still
deploys, but nothing consumes it. See [Consume from Kafka](../guides/kafka-channels.md).
The other is `c4-tournament`: `deploy.sh` installs its plugin, connector,
workflows and channels, then prints the registration of its model entrant —
a bucket, `orion-cli models create --wait`, `models activate` — instead of
sending the request, because a model's bytes are fetched from object storage
by the node rather than posted to it. See [Serve a Model](../build/models.md).

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
| [`kafka-order-events`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/kafka-order-events) | topic `orders.events` | **Kafka ingress:** consumes a topic, stamps the record's coordinates — **needs `kafka.enabled = true` and a broker** |
| [`fixed-width-statement`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/fixed-width-statement) | `POST /statements` | **Plugin-backed:** a fixed-width codec compiled to WebAssembly decodes the line, a `map` summarises it — **needs `plugins.enabled = true`**; the codec's source is in [`examples/plugins/fixed-width/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/plugins/fixed-width) |
| [`order-summary`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary) | `POST /order-summary` | The dependency-free files behind [Understand the HTTP Flow](./first-service.md) |
| [`nightly-rollup`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/nightly-rollup) | `0 15 2 * * *` (no route) | **Scheduled:** a `protocol: "cron"` channel runs the workflow on a six-field expression; every instant becomes a durable occurrence — see [Run work on a schedule](../guides/scheduled-workflows.md) |
| [`c4-tournament`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/c4-tournament) | `POST /c4/register`, `/c4/turn`, `/c4/match`, `GET /c4/leaderboard`, an hourly round | **Model-backed:** a Connect Four tournament for tiny ONNX networks — `model_infer` routes each turn to the entrant to move, a WebAssembly plugin referees, a `loop` plays the match, a SQLite leaderboard keeps score — **needs `models.enabled = true` and `plugins.enabled = true`**, and the entrant registered from a bucket; see [Serve a Model](../build/models.md) |

## What is in a package directory

| File | Sent to | Purpose |
|------|---------|---------|
| `workflow.json` | `POST /api/v1/admin/workflows` | The task pipeline — the logic |
| `workflow-<name>.json` *(optional)* | `POST /api/v1/admin/workflows` | Additional workflows, when the package is more than one service |
| `channel.json` | `POST /api/v1/admin/channels` | The endpoint that routes to the workflow |
| `channel-<name>.json` *(optional)* | `POST /api/v1/admin/channels` | Additional channels |
| `request.json` | `POST /api/v1/data/<route>` | A sample request to try it |
| `connector.json` *(optional)* | `POST /api/v1/admin/connectors` | A named connection to an external system, when the package needs one |
| `plugin.toml` + the component it names *(optional)* | `POST /api/v1/admin/plugins` | A WebAssembly plugin the workflows call, uploaded and activated before them — needs `plugins.enabled = true` |
| `entrant/model.json` + the artifact it names *(optional)* | `orion-cli models create` | A model the workflows call. Not deployed by `deploy.sh`: the bytes go in a bucket behind a `storage` connector and the model is registered from there — needs `models.enabled = true`; the script prints the commands |

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

## Clean up or run another example

`deploy.sh` is repeatable: it skips existing definitions and sends the sample
request again. To avoid identifier collisions while experimenting, use a fresh
local SQLite database or remove the example's channel before its workflow with
`orion-cli ... delete --yes`. Connector-backed package READMEs name their
Docker volumes and cleanup commands.

## Next steps

- [Test & Promote a Service](./test-and-promote.md): take one of these
  packages from a local run to a second instance.
- [Your First Connector](./first-connector.md): build `postgres-orders` by
  hand, one step at a time.
- [Workflow JSON Schema](../reference/workflows.md) and
  [Task Functions](../reference/functions.md): the reference behind every file
  above.
