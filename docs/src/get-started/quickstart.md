<!-- description: Start Orion in Docker, post one workflow and one channel, and call your first live endpoint. Under five minutes, with nothing to build or deploy. -->
<!-- type: quickstart -->
<!-- last_verified: 2026-09-14 -->

# Quickstart

Start Orion, define one service as two JSON documents, and call it over HTTP. The service flags any order over $10,000. The six commands below run in under 5 seconds once the image is local; the first run also pulls 204 MB.

## Before you start

Tested with Orion 1.9.0. You need:

- Docker Engine or Docker Desktop
- `curl`
- port `8080` free on your machine

The commands use a POSIX shell. On Windows, run them from WSL.

## 1. Start Orion

Run the published image; its database is embedded, so there is nothing else to start:

```bash
docker run --name orion-quickstart -d -p 8080:8080 \
  ghcr.io/goplasmatic/orion:latest
```

Wait until the server answers its liveness probe:

```bash
curl --retry 10 --retry-delay 1 --retry-all-errors -fsS \
  http://localhost:8080/healthz
```

Output:

```json
{"status":"ok"}
```

## 2. Create the workflow

A workflow is the logic. This one has two tasks: parse the request, then flag it when `total` is over 10,000. Post it to the admin API:

```bash
curl -fsS -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' \
  -d '{
    "workflow_id": "quickstart-orders",
    "name": "High-value order",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse payload",
        "function": { "name": "parse_json",
                      "input": { "source": "payload", "target": "order" } } },
      { "id": "flag", "name": "Flag order",
        "condition": { ">": [{ "var": "data.order.total" }, 10000] },
        "function": { "name": "map", "input": { "mappings": [
          { "path": "data.order.flagged", "logic": true },
          { "path": "data.order.alert",
            "logic": { "cat": ["High-value order: $",
                               { "var": "data.order.total" }] } }
        ] } } }
    ]
  }'
```

The response echoes the definition with `"status": "draft"`. A draft serves no traffic and never touches the running engine.

## 3. Activate the workflow

Activation is a status change:

```bash
curl -fsS -X PATCH \
  http://localhost:8080/api/v1/admin/workflows/quickstart-orders/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Orion builds a new engine with this workflow in it and swaps it in. Nothing restarts and no request is dropped.

## 4. Create and activate the channel

A channel is the endpoint. It binds a route to the workflow, and it can only be activated once that workflow is active:

```bash
curl -fsS -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' \
  -d '{ "channel_id": "orders", "name": "orders", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/orders",
        "methods": ["POST"], "workflow_id": "quickstart-orders" }'

curl -fsS -X PATCH http://localhost:8080/api/v1/admin/channels/orders/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

## 5. Call it

Send an order over the threshold to the route you declared:

```bash
curl -fsS -X POST http://localhost:8080/api/v1/data/orders \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

Output, with an `id` that differs on every call:

```json
{
  "id": "019febae-d01f-7c31-b6f3-671a42a4a74e",
  "status": "ok",
  "data": {
    "order": {
      "order_id": "ORD-9182",
      "total": 25000,
      "flagged": true,
      "alert": "High-value order: $25000"
    }
  },
  "errors": []
}
```

## Verify

Send an order under the threshold and confirm the `flag` task stays quiet:

```bash
curl -fsS -X POST http://localhost:8080/api/v1/data/orders \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-0001", "total": 50 } }'
```

The `data.order` object comes back with `order_id` and `total` only. There is no `flagged` and no `alert`, because the task's condition was false and the task did not run.

## What just happened

You posted two definitions and Orion did the rest. The workflow holds the logic as an ordered list of tasks, each with an optional JSONLogic condition. The channel binds a route to that workflow and answers the request. Everything around them is the runtime's: routing, the draft-then-activate lifecycle, hot reload, validation, and the trace behind that `id`. It is the same for every service you put on it.

Congratulations: you have a live Orion service.

## Clean up

Stop and remove the container:

```bash
docker stop orion-quickstart && docker rm orion-quickstart
```

The container held its database internally, so removing it removes the definitions too.

## Next steps

- [Build your first service](./tutorials/first-service.md): the same four calls one at a time, with the CLI beside each and what every response means.
- [How Orion works](../concepts/how-orion-works.md): the three primitives and one request's journey through them.
- [Install and run Orion](./install.md): the binary on your own machine, without Docker.
- [Is Orion right for you?](./decide/is-orion-right-for-you.md): the neighbouring tools, and the cases where Orion is the wrong choice.
