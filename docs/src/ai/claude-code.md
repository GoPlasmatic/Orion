# Orion + Claude Code

The fastest way to experience "AI writes services, not code": connect [Claude
Code](https://claude.com/claude-code) to Orion through the [MCP
server](./mcp-setup.md) and describe the service you want. Claude drafts the
workflow, dry-runs it against sample data, activates it, and wires up the
endpoint — while Orion's lifecycle rules (draft → test → activate, immutable
versions, instant rollback) keep every step reversible.

This page is a 10-minute guided session. In it, you will:

- register the Orion MCP server with Claude Code,
- have Claude build, test, and deploy a service from one paragraph of English,
- inspect what it deployed using real trace data,
- change the logic behind a canary rollout, and roll it back.

## Setup

With [Orion and the CLI installed](../getting-started/install.md) and a server
running, register the MCP server:

```bash
claude mcp add orion --env ORION_SERVER_URL=http://localhost:8080 -- orion-cli mcp serve
```

Start `claude` and run `/mcp` — `orion` should be listed with its tools
available. To share the setup with your team, commit a `.mcp.json` instead; that
form and every other client are in [MCP Server Setup](./mcp-setup.md).

## Build a service by describing it

Paste this into Claude Code:

> Create an Orion workflow called `order-triage` that parses incoming orders,
> flags any order over $10,000 with an alert message, and adds a
> `risk_level` field ("high" above 10000, "normal" otherwise). Test it with a
> realistic sample order **before** activating. Then create a REST channel
> `POST /orders` that uses it, activate everything, and send a $25,000 test
> order through it.

Watch the tool calls as Claude works. The sequence it follows is the same safe
path you'd follow by hand:

1. `workflows_create` — the logic lands as a **draft**; nothing serves traffic yet
2. `workflows_test` — dry-run with sample data; the response shows which tasks
   ran and what each produced
3. `workflows_activate` — draft goes live; the engine hot-reloads
4. `channels_create` + `channels_activate` — `POST /orders` now routes to it
5. `data_send_sync` — the test order comes back flagged, with the alert message

Your service is live. Total code written: zero.

## Inspect and operate

Everything Orion records is queryable through the same session:

> Show me the recent traces for the orders channel. What did each task do on
> the last request?

> Is the engine healthy? How many workflows and channels are active?

Claude answers from `traces_list` / `traces_get` / `engine_status` — real
observability data, not summaries of what it *thinks* it deployed.

## Change it safely

Orion's governance makes iteration safe to delegate. Try:

> Lower the flag threshold to $5,000. Create a new version, dry-run it with an
> order at $7,500, and roll it out to 10% of traffic first.

Active versions are immutable, so Claude must create a new version
(`workflows_create_version`), test it, and use `workflows_rollout` for the
canary. If anything looks wrong:

> Roll the orders workflow back to the previous version.

— one tool call, instant.

## Where to go next

- [MCP Server Setup](./mcp-setup.md) — the tool catalogue, the other clients
  (Claude Desktop, Cursor), and the HTTP transport for remote ones.
- [Prompt Pack (any LLM)](./prompt-pack.md) — the same powers over the plain
  REST API, for assistants without MCP.
- [The Entity Lifecycle](../concepts/lifecycle.md) — the draft/active/immutable
  rules that make delegating this safe.
- [Worked Examples: Prompt to Service](../guides/worked-examples.md) — the
  prompts behind four shipped services, with the JSON each produced.
