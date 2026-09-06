<!-- description: Build your first Orion service with four administration calls, then send a request to its live REST endpoint and inspect the response. -->
# Understand the HTTP Flow

**Page type:** Tutorial · **Audience:** Developers who completed the quickstart

**Tested with:** Orion 1.7.0 · **Last reviewed:** 2026-09-04

An Orion service consists of a **workflow** that says what to do and a
**channel** that says where to reach it, plus optional connectors for external
systems. This tutorial creates the workflow and channel against the server you
just started, activates them, and calls the result.

In this guide, you will:

- create a workflow that adds a summary to an incoming order,
- expose it at `POST /api/v1/data/order-summary`,
- send a request and read the response back.

Nothing here is compiled or deployed. Four administration calls create and
activate the definitions; a fifth, data-plane call invokes the live endpoint.

## Before you start

You need:

- a running local Orion server, installed and verified with
  [Install & Run](./install.md),
- `curl` and a POSIX-compatible shell for the commands below,
- `jq` for the optional verification command.

The examples assume Orion is available at `http://localhost:8080`. Windows
PowerShell users can follow the same operations in the
[Console](./console.md) or use the CLI equivalents later on this page. Those
equivalents require `orion-cli` from the installation guide.

<div class="asciinema-player" data-cast="casts/quickstart.cast"></div>
<span class="asciinema-caption">▶ Click to play. The whole tutorial, over plain HTTP.</span>

> [!TIP]
> Prefer point-and-click? [Orion Console](./console.md) walks the same
> flow entirely in the browser.
>
> In a hurry? The [Quickstart](./quickstart.md) downloads the tested setup
> script for inspection, deploys a ready-made orders service, and sends it a
> request. The steps below build a smaller orders service (`order-summary`) by hand so
> you can see each moving part.

## 1. Create the workflow

The workflow parses the incoming payload, then writes one new field derived from it. Two tasks, run in order:

The request bodies below are complete. They are also available as copyable,
tested files in
[`examples/packages/order-summary/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/order-summary).

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

It is saved as a **draft**. Drafts serve no traffic and never touch the running engine, so nothing you do here can affect a live endpoint.

## 2. Activate it

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-summary/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

Activation triggers a hot reload: Orion builds a new engine and swaps it in. In-flight requests finish on the engine they started with, so there is no restart and no dropped traffic.

## 3. Create and activate the channel

The channel is the endpoint. It names the route, the methods it answers, and the workflow it runs:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "order-summary", "name": "order-summary", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/order-summary",
        "methods": ["POST"], "workflow_id": "order-summary" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/order-summary/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

A channel can only be activated once its workflow is active — the endpoint can never point at logic that is not serving.

## 4. Call it

Send a request to the endpoint you just created:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/order-summary \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-42", "total": 125 } }'
```

Expected JSON response:

```json
{
  "id": "019febae-d01f-7c31-b6f3-671a42a4a74e",
  "status": "ok",
  "data": { "req": { "order_id": "ORD-42", "total": 125, "summary": "Order ORD-42: $125" } },
  "errors": []
}
```

That is the whole service. Requests arrive under `{"data": …}`, `parse_json`
lifts the payload into the data context at `data.req`, `map` writes
`data.req.summary`, and the finished context is returned. `id` is the trace id
for this execution — the handle you would poll on an async channel, and the key
you look a request up by later.

## Verify it

Two checks confirm the service is really live, not just accepted:

```bash
curl -s http://localhost:8080/health | jq '.workflows_loaded'
orion-cli channels list
```

- The health response returns `"workflows_loaded": 1`. This field counts what the
  *running engine* holds, so it moves only when an activation has actually
  reloaded the engine — a workflow that was created but never activated leaves
  it at `0`.
- `orion-cli channels list` displays the active `order-summary` channel and its associated workflow.

## The same flow with the CLI

Every call above has a CLI equivalent, and `orion-cli send` replaces the
data-plane curl. Save the workflow and channel request bodies above as
`workflow.json` and `channel.json`, then run:

```bash
orion-cli workflows create -f workflow.json
orion-cli workflows activate order-summary
orion-cli channels create -f channel.json
orion-cli channels activate order-summary
orion-cli send order-summary -d '{ "order_id": "ORD-42", "total": 125 }'
```

Pass the bare business payload to `send`; the CLI wraps it in Orion's request
envelope. See [`orion-cli send`](../reference/cli.md#send) for raw body mode,
metadata, asynchronous submission, and other payload details.

<div class="asciinema-player" data-cast="casts/cli-lifecycle.cast"></div>
<span class="asciinema-caption">▶ Click to play. Create, activate, dry-run, then live traffic — one tool.</span>

## Clean up or run it again

Re-running the create calls returns `409 CONFLICT` because the identifiers
already exist; invoking the active endpoint remains safe. To repeat the full
creation flow on the same instance, remove the channel before its workflow:

```bash
orion-cli channels delete order-summary --yes
orion-cli workflows delete order-summary --yes
```

These commands permanently remove every stored version of the two tutorial
definitions. Skip them if you want to continue into testing and promotion.

## Next steps

- [Packages](../concepts/packages.md): you created two individual definitions;
  keep them together as a versioned package for source control and promotion.
- [Your First Connector](./first-connector.md): the same shape of service, but reading and writing a real PostgreSQL database.
- [Test & Promote a Service](./test-and-promote.md): test this workflow offline, then ship it to a second instance as a versioned package.
- [Build a Service with Claude Code](../ai/claude-code.md): hand the four steps above to an AI assistant and describe what you want instead.
- [Secure an Instance](../operate/security.md): The data plane you just called
  does not authenticate. Read this before anything you do not control can reach
  it.
