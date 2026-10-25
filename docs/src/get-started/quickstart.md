<!-- description: Start Orion in Docker, upload one workflow and one channel, deploy them, and call your first live endpoint. Under five minutes, with nothing to build. -->
<!-- type: quickstart -->
<!-- last_verified: 2026-10-25 -->

# Quickstart

Start Orion, define one service as two JSON documents, and call it over HTTP. The service flags any order over $10,000. The commands below run in under 5 seconds once the image is local; the first run also pulls 204 MB.

## Before you start

Tested with Orion 1.12.0. You need:

- Docker Engine or Docker Desktop
- `curl` and `jq`
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

## 2. Upload the workflow

A workflow is the logic. This one has two tasks: parse the request, then flag it when `total` is over 10,000. Upload it to the admin API and keep the digest it answers:

```bash
WF=$(curl -fsS -X POST http://localhost:8080/api/v2/admin/definitions \
  -H 'Content-Type: application/json' \
  -d '{"kind": "workflow", "definition": {
    "name": "High-value order",
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
  }}' | jq -r .digest)
```

The server validates the definition the way a node loads it, and answers `{"digest": "sha256:…", "kind": "workflow", …}`. Uploading serves nothing yet. A definition is stored by its content, and has no name or version until a deployment gives it one.

## 3. Upload the channel

A channel is the endpoint. It binds a route to a workflow. The binding lives in the deployment, not in the channel:

```bash
CH=$(curl -fsS -X POST http://localhost:8080/api/v2/admin/definitions \
  -H 'Content-Type: application/json' \
  -d '{"kind": "channel", "definition": {
    "name": "orders", "channel_type": "sync", "protocol": "rest",
    "route_pattern": "/orders", "methods": ["POST"] }}' | jq -r .digest)
```

## 4. Deploy them

A deployment names everything the node serves. This one deploys the workflow as `quickstart-orders` and the channel as `orders`, and says the channel requires the workflow:

```bash
curl -fsS -X POST http://localhost:8080/api/v2/admin/deployments \
  -H 'Content-Type: application/json' \
  -d '{"idempotency_key": "quickstart-1", "base_revision": 0, "reason": "quickstart",
    "manifest": { "schema_version": 2, "providers": [], "members": [
      { "entity": { "kind": "workflow", "id": "quickstart-orders", "revision": 1 },
        "definition_digest": "'"$WF"'" },
      { "entity": { "kind": "channel", "id": "orders", "revision": 1 },
        "definition_digest": "'"$CH"'",
        "requires": [{ "kind": "workflow", "id": "quickstart-orders", "revision": 1 }] }
    ] } }'
```

The answer carries `"committed_revision": 1` and an `evaluation` with `"converged": true`. Orion built a new engine from the deployment and swapped it in. Nothing restarts and no request is dropped. `base_revision: 0` means "only if nothing is deployed yet", so two deploys racing on one instance cannot both win.

## 5. Call it

Send an order over the threshold to the route you declared:

```bash
curl -fsS -X POST http://localhost:8080/api/v2/data/orders \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-9182", "total": 25000 } }'
```

Output, with an `id` that differs on every call:

```json
{
  "id": "01a1738f-4070-70ba-abbb-e76b63dd57e7",
  "status": "ok",
  "data": {
    "order_id": "ORD-9182",
    "total": 25000,
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
curl -fsS -X POST http://localhost:8080/api/v2/data/orders \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-0001", "total": 50 } }'
```

The `data.order` object comes back with `order_id` and `total` only. There is no `flagged` and no `alert`, because the task's condition was false and the task did not run.

## What just happened

You uploaded two definitions, deployed them, and Orion did the rest. The workflow holds the logic as an ordered list of tasks, each with an optional JSONLogic condition. The channel binds a route to that workflow and answers the request. Everything around them is the runtime's: routing, numbered deployments with rollback, hot reload, validation, and the trace behind that `id`. It is the same for every service you put on it.

Congratulations: you have a live Orion service.

## Clean up

Stop and remove the container:

```bash
docker stop orion-quickstart && docker rm orion-quickstart
```

The container held its database internally, so removing it removes the definitions too.

## Next steps

- [Build your first service](./tutorials/first-service.md): the same service as files, compiled and deployed with the CLI.
- [How Orion works](../concepts/how-orion-works.md): the three primitives and one request's journey through them.
- [Install and run Orion](./install.md): the binary on your own machine, without Docker.
- [Is Orion right for you?](./decide/is-orion-right-for-you.md): the neighbouring tools, and the cases where Orion is the wrong choice.
