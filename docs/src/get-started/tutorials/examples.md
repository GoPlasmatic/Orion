<!-- description: Ready-to-deploy Orion example packages, such as order classification, webhook transforms and scheduled rollups, each compiled and applied in two commands. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-10-25 -->

# Run the example packages

The repository ships ready-to-deploy services, each a *package*: one directory holding a service's channels, workflows and connector. This tutorial compiles one into a deployment and applies it.

## What you will learn

- What files a package directory holds, and what each one becomes in a deployment.
- How `compile` and `orion-cli deployments apply -d` deploy a directory.
- Which examples need a plugin sandbox, and which need nothing.
- Where the offline regression suite for the examples lives.

## Before you start

Tested with Orion 1.12.0. You need:

- Git, `curl`, `orion-server` and `orion-cli`
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

With the server running, compile the package and apply it:

```bash
orion-cli compile packages/high-value-order -o /tmp/high-value-order
orion-cli deployments apply -d /tmp/high-value-order
```

`compile` checks the directory and writes a deployment directory. It skips `request.json`, with a note, because that file is not a definition. `apply` uploads the definitions, prints the plan, and commits it as the server's next revision. Re-running it is safe: a directory the head already deploys is reported as up to date.

A deployment is everything the node serves. Applying a second package removes the first, and the plan shows that as `-` lines. To run several examples side by side, copy their files into one directory and compile that.

Then send the sample request:

```bash
curl -X POST http://localhost:8080/api/v2/data/high-value-orders \
  -H 'Content-Type: application/json' --data @packages/high-value-order/request.json
```

A package with no HTTP route deploys the same way. `nightly-rollup` is a cron channel: it runs on its schedule instead of answering a request.

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
| [`fixed-width-statement`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/fixed-width-statement) | `POST /statements` | **Plugin-backed:** a fixed-width codec compiled to WebAssembly decodes the line, a `map` summarizes it. Needs `plugins.enabled = true`; the codec's source is in [`examples/plugins/fixed-width/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/plugins/fixed-width) |
| [`c4-referee`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/c4-referee) | `POST /c4/move` | **Plugin-backed:** a Connect Four referee compiled to WebAssembly applies one move to a board and returns the view of the new state. Needs `plugins.enabled = true`; the referee's source is in [`examples/plugins/c4-rules/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/plugins/c4-rules) |
| [`order-summary`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary) | `POST /order-summary` | The dependency-free files behind [Build your first service](./first-service.md) |
| [`nightly-rollup`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/nightly-rollup) | `0 15 2 * * *` (no route) | **Scheduled:** a `protocol: "cron"` channel runs the workflow on a six-field expression, and every instant becomes a durable occurrence. See [Run work on a schedule](../../guides/patterns/scheduled-workflows.md) |

## What is in a package directory

| File | Becomes | Purpose |
|------|---------|---------|
| `workflow.json` | a workflow definition | The task pipeline, which is the logic |
| `workflow-<name>.json` *(optional)* | a workflow definition | Additional workflows, when the package is more than one service |
| `channel.json` | a channel definition | The endpoint that routes to the workflow |
| `channel-<name>.json` *(optional)* | a channel definition | Additional channels |
| `request.json` | nothing; `compile` skips it | A sample request to send to `POST /api/v2/data/<route>` |
| `connector.json` *(optional)* | a connection revision | A named connection to an external system, when the package needs one |
| `plugin.toml` + the component it names *(optional)* | a plugin definition, component inlined | A WebAssembly plugin the workflows call. Needs `plugins.enabled = true` |

[Deployment directories](../../reference/cli/orion-cli/compile.md#deployment-directories) lists exactly what `compile` writes for each.

> [!NOTE]
> Requests use the `{ "data": { … } }` envelope. Orion unwraps `data` into the workflow payload, which `parse_json` reads with `"source": "payload"`.

> [!TIP]
> To run a workflow on the server once without deploying it, send it to [`POST /api/v2/admin/definitions/test`](../../reference/admin-api/deployments.md#test-a-workflow-definition). The answer carries an execution trace showing which tasks ran or were skipped. Offline, `orion-cli dry-run` does the same with no server at all.

## The offline test suite

[`examples/workflow-tests/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/workflow-tests) holds `*.case.json` regression cases for the self-contained packages. Each runs the real workflow JSON through the real engine with no server, database or network:

```bash
orion-cli test examples/workflow-tests \
  --plugin-dir examples/packages/fixed-width-statement \
  --plugin-dir examples/packages/c4-referee
```

The two `--plugin-dir` flags hand the runner the plugins two packages call, which run for real. It exits non-zero on any failure, so it gates CI beside `orion-cli lint`. [`examples/use-cases/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/use-cases) does the same job against a real server: the repository's e2e suite deploys each case through `orion-cli` and asserts the live responses.

## Clean up or run another example

Applying the same directory again does nothing, and sending the sample request again is always safe. Applying another package's directory replaces this one, so moving between examples needs no cleanup. To start from nothing, use a fresh local SQLite database. Connector-backed package READMEs name their Docker volumes and cleanup commands.

## Recap

- A package directory is a set of definitions plus a sample request.
- `compile` turns it into a deployment directory, and `orion-cli deployments apply -d` deploys it as one revision.
- The self-contained packages need nothing but a server. Plugin packages need `plugins.enabled = true`.
- `examples/workflow-tests/` proves the packages offline; `examples/use-cases/` proves them against a live server.

## Next steps

- [Test and deploy a service](./test-and-promote.md): prove one of these packages offline, then deploy it as one reviewed revision.
- [Add your first connector](./first-connector.md): build `postgres-orders` by hand, one step at a time.
- [Workflow definition](../../reference/workflows.md) and [Task functions](../../reference/functions/index.md): the reference behind every file above.
