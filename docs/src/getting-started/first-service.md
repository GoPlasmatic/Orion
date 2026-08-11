# Your First Service

A service in Orion is two JSON documents: a **workflow** that says what to do, and a **channel** that says where to reach it. This tutorial creates both against the server you just started, activates them, and calls the result.

In this guide, you will:

- create a workflow that greets whoever it is sent,
- expose it at `POST /api/v1/data/hello`,
- send a request and read the response back.

Nothing here is compiled or deployed. Every step is one API call, and the endpoint is live the moment you activate it.

<div class="asciinema-player" data-cast="casts/quickstart.cast"></div>
<span class="asciinema-caption">▶ Click to play. The whole tutorial, over plain HTTP.</span>

> [!TIP]
> Prefer point-and-click? [The Console (Orion UI)](./console.md) walks the same
> flow entirely in the browser.
>
> In a hurry? `curl -fsSL https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh | bash`
> runs all four steps against your instance and sends the test request. The steps
> below build the same service by hand, so you can see each moving part.

## 1. Create the workflow

The workflow parses the incoming payload, then writes one new field derived from it. Two tasks, run in order:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_id": "hello-world",
    "name": "Hello World",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse", "function": {
          "name": "parse_json", "input": { "source": "payload", "target": "req" }
      }},
      { "id": "greet", "name": "Greet", "function": {
          "name": "map", "input": { "mappings": [
            { "path": "data.req.greeting", "logic": {
              "cat": ["Hello, ", { "var": "data.req.name" }, "!"]
            }}
          ]}
      }}
    ]
  }'
```

It is saved as a **draft**. Drafts serve no traffic and never touch the running engine, so nothing you do here can affect a live endpoint.

## 2. Activate it

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/hello-world/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

Activation triggers a hot reload: Orion builds a new engine and swaps it in. In-flight requests finish on the engine they started with, so there is no restart and no dropped traffic.

## 3. Create and activate the channel

The channel is the endpoint. It names the route, the methods it answers, and the workflow it runs:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H "Content-Type: application/json" \
  -d '{ "channel_id": "hello", "name": "hello", "channel_type": "sync",
        "protocol": "rest", "route_pattern": "/hello",
        "methods": ["POST"], "workflow_id": "hello-world" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/hello/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
```

A channel can only be activated once its workflow is active — the endpoint can never point at logic that is not serving.

## 4. Call it

Send a request to the endpoint you just created:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/hello \
  -H "Content-Type: application/json" \
  -d '{ "data": { "name": "World" } }'
```

Expected JSON response:

```json
{
  "id": "019febae-d01f-7c31-b6f3-671a42a4a74e",
  "status": "ok",
  "data": { "req": { "name": "World", "greeting": "Hello, World!" } },
  "errors": []
}
```

That is the whole service. Requests arrive under `{"data": …}`, `parse_json` lifts the payload into the data context at `data.req`, `map` writes `data.req.greeting`, and the finished context is returned. `id` is the trace id for this execution — the handle you would poll on an async channel, and the key you look a request up by later.

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
- `orion-cli channels list` displays the active `hello` channel and its associated workflow.

## The same flow with the CLI

Every call above has a CLI equivalent, and `orion-cli send` replaces the data-plane curl:

```bash
orion-cli workflows create -f workflow.json
orion-cli workflows activate hello-world
orion-cli channels create -f channel.json
orion-cli channels activate hello
orion-cli send hello -d '{ "data": { "name": "World" } }'
```

<div class="asciinema-player" data-cast="casts/cli-lifecycle.cast"></div>
<span class="asciinema-caption">▶ Click to play. Create, activate, dry-run, then live traffic — one tool.</span>

## Next steps

- [Your First Connector](./first-connector.md) — the same shape of service, but reading and writing a real PostgreSQL database.
- [Test & Promote a Service](./test-and-promote.md) — test this workflow offline, then ship it to a second instance as a versioned package.
- [Build a Service with Claude Code](../ai/claude-code.md) — hand the four steps above to an AI assistant and describe what you want instead.
- [Secure an Instance](../operate/security.md) — The data plane you just called
  does not authenticate. Read this before anything you do not control can reach
  it.
