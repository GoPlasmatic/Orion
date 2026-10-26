<!-- description: Build your first Orion service by hand: write a workflow and a channel, compile them into a deployment, apply it, then call the endpoint. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-10-25 -->

# Build your first service

An Orion service is a workflow that says what to do and a channel that says where to reach it. This tutorial writes both as files, deploys them to a running server, and calls the result.

## What you will learn

- What a workflow and a channel each declare, and why they are two documents.
- How a directory of definitions becomes a deployment the server serves.
- How to read a data-plane response, and what its `id` is for.

## Before you start

Tested with Orion 1.12.0. You need:

- an Orion server on `http://localhost:8080`, from [Install and run Orion](../install.md)
- `orion-server` and `orion-cli` on your `PATH`
- `curl` and a POSIX shell (on Windows, WSL)
- `jq`, for the verification step

The service is smaller than the quickstart's: it adds one summary field to an incoming order. Every file is complete as written, and the same files ship as [`examples/packages/order-summary/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary).

## 1. Write the workflow

The workflow parses the incoming payload, then writes one new field derived from it. Two tasks, run in order. Save it as `order-summary/workflow.json`:

```json
{
  "workflow_id": "order-summary",
  "name": "Order Summary",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "req" }
    }},
    { "id": "summarize", "name": "Summarize", "function": {
        "name": "map", "input": { "mappings": [
          { "path": "data.req.summary", "logic": {
            "cat": ["Order ", { "var": "data.req.order_id" }, ": $", { "var": "data.req.total" }]
          }}
        ]}
    }}
  ]
}
```

## 2. Write the channel

The channel is the endpoint. It names the route, the methods it answers, and the workflow it runs. Save it as `order-summary/channel.json`:

```json
{
  "channel_id": "order-summary",
  "name": "order-summary",
  "channel_type": "sync",
  "protocol": "rest",
  "route_pattern": "/order-summary",
  "methods": ["POST"],
  "workflow_id": "order-summary"
}
```

## 3. Compile and deploy

`compile` checks the directory the way the server would and writes it as a deployment directory. `deployments apply -d` uploads it and commits it as the server's next revision:

```bash
orion-cli compile order-summary -o dist/deployment
orion-cli deployments apply -d dist/deployment
```

`compile` prints `order-summary: 0 connector(s), 1 workflow(s), 1 channel(s) — 0 error(s), 0 warning(s)`. `apply` prints the plan first: `+ channel order-summary@1` and `+ workflow order-summary@1`. It asks before it commits; `--yes` skips the prompt.

The server builds a new engine from the deployment and swaps it in. Requests in flight finish on the engine they started with, so nothing restarts and no traffic is dropped. A deployment is everything the node serves: anything an earlier deployment carried that this one does not shows as removed (`-`) in the plan.

## 4. Call it

Send a request to the route you declared:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X POST http://localhost:8080/api/v2/data/order-summary \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-42", "total": 125 } }'
```

</section>
<section data-tab="CLI">

```bash
orion-cli send order-summary -d '{ "order_id": "ORD-42", "total": 125 }'
```

</section>
</div>

Output, with an `id` that differs on every call:

```json
{
  "id": "019febae-d01f-7c31-b6f3-671a42a4a74e",
  "status": "ok",
  "data": {
    "order_id": "ORD-42", "total": 125,
    "req": { "order_id": "ORD-42", "total": 125, "summary": "Order ORD-42: $125" }
  },
  "errors": []
}
```

Requests arrive under `{"data": …}`. `parse_json` lifts the payload into the data context at `data.req`, `map` writes `data.req.summary`, and the finished context comes back. The `id` is the trace id of this execution. It is the handle you poll on an async channel, and the key you look a request up by later. `orion-cli send` takes the bare business payload and wraps it in that envelope for you.

## Verify

Two checks confirm the service is live, not only accepted:

```bash
curl -s http://localhost:8080/health | jq '.workflows_loaded'
curl -s http://localhost:8080/api/v2/admin/deployments | jq '{head_revision, applied_revision}'
```

The health response reports `"workflows_loaded": 1`. That field counts what the running engine holds, so it moves only when a deployment has been built. The second shows the head revision and the revision this node has applied; they match once the node serves it.

## Clean up or run it again

Re-running the same `apply` does nothing: the CLI reports the head as up to date. Calling the endpoint again is always safe. To take the service down, [roll back](../../reference/admin-api/deployments.md#rollback) to an earlier revision, or deploy a directory that does not carry it. Remove the files with `rm -ri order-summary dist`, or keep them if you are continuing to the next tutorial.

## Recap

- A workflow is an ordered list of tasks; a channel binds a route to one workflow. They are separate documents because one workflow can serve several channels.
- Definitions are files. `compile` turns a directory of them into a deployment, and only applying a deployment changes what the engine serves.
- A data-plane response returns the finished data context plus an `id`, which is the trace of that run.
- `orion-cli deployments apply -d` is a thin client over the [deployment endpoints](../../reference/admin-api/deployments.md); the [quickstart](../quickstart.md) makes the same calls with `curl`.

## Next steps

- [Add your first connector](./first-connector.md): the same shape of service, reading and writing a real PostgreSQL database.
- [Test and deploy a service](./test-and-promote.md): test a workflow offline, then deploy it as one reviewed revision.
- [Deployments](../../concepts/deployments.md): the manifest, the head and the build check behind `apply -d`.
- [Deployment endpoints](../../reference/admin-api/deployments.md): what `apply -d` does on the server, and how to roll back.
- [Secure an instance](../../operate/run/security.md): the data plane you called does not authenticate; read this before anything you do not control can reach it.
