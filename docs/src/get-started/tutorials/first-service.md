<!-- description: Build your first Orion service by hand: create and activate a workflow and a channel in four administration calls, then call the endpoint and read the response. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Build your first service

An Orion service is a workflow that says what to do and a channel that says where to reach it. This tutorial creates both against a running server, activates them, and calls the result. The `curl` and CLI form of every step sit side by side.

## What you will learn

- What a workflow and a channel each declare, and why they are two documents.
- What the draft-then-activate lifecycle protects you from.
- How to read a data-plane response, and what its `id` is for.
- How the CLI maps onto the admin API.

## Before you start

Tested with Orion 1.8.1. You need:

- an Orion server on `http://localhost:8080`, from [Install and run Orion](../install.md)
- `curl` and a POSIX shell (on Windows, WSL)
- `jq`, for the verification step
- `orion-cli`, if you want the CLI tab of each step

The service is smaller than the quickstart's: it adds one summary field to an incoming order. Every request body is complete as written, and the same files ship as [`examples/packages/order-summary/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary).

<div class="asciinema-player" data-cast="casts/quickstart.cast"></div>
<span class="asciinema-caption">The whole tutorial over plain HTTP. Click to play.</span>

## 1. Create the workflow

The workflow parses the incoming payload, then writes one new field derived from it. Two tasks, run in order:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
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
  }'
```

</section>
<section data-tab="CLI">

Save the JSON body from the `curl` tab as `workflow.json`, then:

```bash
orion-cli workflows create -f workflow.json
```

</section>
</div>

The response carries `"status": "draft"`. A draft serves no traffic and never touches the running engine, so nothing you do here can affect a live endpoint.

## 2. Activate the workflow

Activation is a status change on the workflow you created:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-summary/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

</section>
<section data-tab="CLI">

```bash
orion-cli workflows activate order-summary
```

</section>
</div>

Activation triggers a hot reload: Orion builds a new engine and swaps it in. Requests in flight finish on the engine they started with, so nothing restarts and no traffic is dropped.

## 3. Create and activate the channel

The channel is the endpoint. It names the route, the methods it answers, and the workflow it runs:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "order-summary", "name": "order-summary", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/order-summary",
        "methods": ["POST"], "workflow_id": "order-summary" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/order-summary/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

</section>
<section data-tab="CLI">

Save the JSON body from the `curl` tab as `channel.json`, then:

```bash
orion-cli channels create -f channel.json
orion-cli channels activate order-summary
```

</section>
</div>

A channel can only be activated once its workflow is active, so an endpoint can never point at logic that is not serving.

## 4. Call it

Send a request to the route you declared:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X POST http://localhost:8080/api/v1/data/order-summary \
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
  "data": { "req": { "order_id": "ORD-42", "total": 125, "summary": "Order ORD-42: $125" } },
  "errors": []
}
```

Requests arrive under `{"data": …}`. `parse_json` lifts the payload into the data context at `data.req`, `map` writes `data.req.summary`, and the finished context comes back. The `id` is the trace id of this execution. It is the handle you poll on an async channel, and the key you look a request up by later. `orion-cli send` takes the bare business payload and wraps it in that envelope for you.

## Verify

Two checks confirm the service is live, not only accepted:

```bash
curl -s http://localhost:8080/health | jq '.workflows_loaded'
orion-cli channels list
```

The health response reports `"workflows_loaded": 1`. That field counts what the running engine holds, so it moves only when an activation has reloaded the engine. A workflow created but never activated leaves it at `0`. `orion-cli channels list` shows `order-summary` as active with its workflow beside it.

<div class="asciinema-player" data-cast="casts/cli-lifecycle.cast"></div>
<span class="asciinema-caption">Create, activate, dry-run, then live traffic, with one tool. Click to play.</span>

## Clean up or run it again

Re-running the create calls answers `409 CONFLICT`, because the identifiers exist. Calling the active endpoint again is always safe. To repeat the whole flow on the same instance, remove the channel before its workflow:

```bash
orion-cli channels delete order-summary --yes
orion-cli workflows delete order-summary --yes
```

Deletion removes every stored version of both definitions. Keep them if you are continuing to the next tutorial.

## Recap

- A workflow is an ordered list of tasks; a channel binds a route to one workflow. They are separate documents because one workflow can serve several channels, and a channel can move between versions.
- Everything is created as a draft. Only an active workflow can back an active channel, and only activation touches the engine.
- A data-plane response returns the finished data context plus an `id`, which is the trace of that run.
- `orion-cli` is a thin client over the admin API: every command above is one of the `curl` calls.

## Next steps

- [Add your first connector](./first-connector.md): the same shape of service, reading and writing a real PostgreSQL database.
- [Test and promote a service](./test-and-promote.md): test this workflow offline, then ship it to a second instance as a versioned package.
- [Packages](../../concepts/packages.md): keep the two definitions together as one versioned unit for source control and promotion.
- [Secure an instance](../../operate/run/security.md): the data plane you called does not authenticate; read this before anything you do not control can reach it.
