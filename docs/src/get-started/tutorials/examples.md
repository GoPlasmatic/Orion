<!-- description: Ready-to-deploy Orion example packages — order classification, webhook transforms, Kafka events, IoT alerts — each with a step-by-step walkthrough. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Run the example packages

The repository ships twelve ready-to-deploy services, each a *package*: one service's channels, workflows and connector, grouped so they deploy and [promote](../../concepts/packages.md) together. This tutorial deploys one, then shows what the deploy script did.

## What you will learn

- What files a package directory holds, and which admin call each one is sent to.
- What `deploy.sh` does, and how to do the same by hand.
- Which examples need a broker, a plugin sandbox or a model, and which need nothing.
- Where the offline regression suite for the examples lives.

## Before you start

Tested with Orion 1.8.0. You need:

- Git, `curl` and Python 3
- an Orion server on `http://localhost:8080`
- a POSIX shell (on Windows, WSL)
- Docker with Compose, for the packages whose descriptions call for it

Most packages are self-contained: built-in functions and JSONLogic only, with no database and no connector. Every workflow here is linted and deployed end to end in CI, so what you copy is what CI proves.

## 1. Get the files

No install method produces a checkout, so start with one:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion/examples
```

## 2. Deploy one

With the server running:

```bash
./deploy.sh high-value-order
```

`deploy.sh` creates and activates every workflow the package ships, then every channel, creating a connector first if the package has one. It then posts `request.json` to the primary channel and prints the response. Re-running it is safe: objects that already exist are skipped.

A package with no HTTP route deploys the same way, and the script prints what it now listens to instead of sending a request. `kafka-order-events` prints its topic and needs a broker plus a server started with `[kafka] enabled = true`; `nightly-rollup` prints its schedule. `c4-tournament` deploys its plugin, connector, workflows and channels, then prints the commands that register its model entrant from a bucket. A model's bytes are fetched by the node rather than posted to it.

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
| [`kafka-order-events`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/kafka-order-events) | topic `orders.events` | **Kafka ingress:** consumes a topic and stamps the record's coordinates. Needs `kafka.enabled = true` and a broker |
| [`fixed-width-statement`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/fixed-width-statement) | `POST /statements` | **Plugin-backed:** a fixed-width codec compiled to WebAssembly decodes the line, a `map` summarizes it. Needs `plugins.enabled = true`; the codec's source is in [`examples/plugins/fixed-width/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/plugins/fixed-width) |
| [`order-summary`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary) | `POST /order-summary` | The dependency-free files behind [Build your first service](./first-service.md) |
| [`nightly-rollup`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/nightly-rollup) | `0 15 2 * * *` (no route) | **Scheduled:** a `protocol: "cron"` channel runs the workflow on a six-field expression, and every instant becomes a durable occurrence. See [Run work on a schedule](../../guides/patterns/scheduled-workflows.md) |
| [`c4-tournament`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/c4-tournament) | `POST /c4/register`, `/c4/turn`, `/c4/match`, `GET /c4/leaderboard`, an hourly round | **Model-backed:** a Connect Four tournament for tiny ONNX networks. `model_infer` routes each turn to the entrant to move, a WebAssembly plugin referees, a `loop` plays the match, a SQLite leaderboard keeps score. Needs `models.enabled = true` and `plugins.enabled = true`, and the entrant registered from a bucket. See [Serve a model](../../guides/extend/models.md) |

## What is in a package directory

| File | Sent to | Purpose |
|------|---------|---------|
| `workflow.json` | `POST /api/v1/admin/workflows` | The task pipeline, which is the logic |
| `workflow-<name>.json` *(optional)* | `POST /api/v1/admin/workflows` | Additional workflows, when the package is more than one service |
| `channel.json` | `POST /api/v1/admin/channels` | The endpoint that routes to the workflow |
| `channel-<name>.json` *(optional)* | `POST /api/v1/admin/channels` | Additional channels |
| `request.json` | `POST /api/v1/data/<route>` | A sample request to try it |
| `connector.json` *(optional)* | `POST /api/v1/admin/connectors` | A named connection to an external system, when the package needs one |
| `plugin.toml` + the component it names *(optional)* | `POST /api/v1/admin/plugins` | A WebAssembly plugin the workflows call, uploaded and activated before them. Needs `plugins.enabled = true` |
| `entrant/model.json` + the artifact it names *(optional)* | `orion-cli models create` | A model the workflows call. Not deployed by `deploy.sh`: the bytes go in a bucket behind a `storage` connector and the model is registered from there. Needs `models.enabled = true`; the script prints the commands |

Every entity carries a `tags: ["pkg:<name>"]` label. That label is what marks it as part of the package, and what package export selects on.

> [!NOTE]
> Requests use the `{ "data": { … } }` envelope. Orion unwraps `data` into the workflow payload, which `parse_json` reads with `"source": "payload"`.

## 3. Deploy one step by step

`deploy.sh` is four API calls and a request. Running them yourself shows the lifecycle each one drives:

```bash
cd packages/high-value-order
```

Create the workflow. It lands as a draft:

```bash
curl -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' --data @workflow.json
```

Activate it:

```bash
curl -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Create the channel, then activate it:

```bash
curl -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' --data @channel.json

curl -X PATCH http://localhost:8080/api/v1/admin/channels/high-value-orders/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Send the sample request:

```bash
curl -X POST http://localhost:8080/api/v1/data/high-value-orders \
  -H 'Content-Type: application/json' --data @request.json
```

> [!TIP]
> A draft workflow can be dry-run against sample data without serving any traffic. `/test` takes the same `{ "data": … }` envelope and returns an execution trace showing which tasks ran or were skipped:
>
> ```bash
> curl -X POST http://localhost:8080/api/v1/admin/workflows/high-value-order/test \
>   -H 'Content-Type: application/json' --data @request.json
> ```

## The offline test suite

[`examples/workflow-tests/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/workflow-tests) holds `*.case.json` regression cases for the self-contained packages. Each runs the real workflow JSON through the real engine with no server, database or network:

```bash
orion-server test examples/workflow-tests
```

It exits non-zero on any failure, so it gates CI beside `orion-server lint`. [`examples/use-cases/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/use-cases) does the same job against a real server: the repository's e2e suite deploys each case through `orion-cli` and asserts the live responses.

## Clean up or run another example

`deploy.sh` is repeatable: it skips existing definitions and sends the sample request again. To avoid identifier collisions while experimenting, use a fresh local SQLite database, or remove the example's channel before its workflow with `orion-cli … delete --yes`. Connector-backed package READMEs name their Docker volumes and cleanup commands.

## Recap

- A package directory is a set of admin-API request bodies plus a sample request, tagged `pkg:<name>`.
- `deploy.sh` posts them in dependency order and activates each; doing it by hand is four calls.
- The self-contained packages need nothing but a server. Kafka, plugin and model packages need the matching runtime setting turned on.
- `examples/workflow-tests/` proves the packages offline; `examples/use-cases/` proves them against a live server.

## Next steps

- [Test and promote a service](./test-and-promote.md): take one of these packages from a local run to a second instance.
- [Add your first connector](./first-connector.md): build `postgres-orders` by hand, one step at a time.
- [Workflow definition](../../reference/workflows.md) and [Task functions](../../reference/functions/index.md): the reference behind every file above.
