# Orion + Claude Code

The fastest way to experience "AI writes services, not code": connect [Claude
Code](https://claude.com/claude-code) to Orion through the [MCP
server](./mcp-setup.md) and describe the service you want. Claude drafts the
workflow, dry-runs it against sample data, activates it, and wires up the
endpoint — while Orion's lifecycle rules (draft → test → activate, immutable
versions, instant rollback) keep every step reversible.

This page is a 10-minute guided session. It assumes nothing beyond a running
Orion instance.

## Setup

1. **Run Orion** (see [Install & First Service](./cli-setup.md)):

   ```bash
   brew install GoPlasmatic/tap/orion-server && orion-server
   ```

2. **Install the CLI**, which contains the MCP server:

   ```bash
   brew install GoPlasmatic/tap/orion-cli
   ```

3. **Register the MCP server with Claude Code** — one command:

   ```bash
   claude mcp add orion --env ORION_SERVER_URL=http://localhost:8080 -- orion-cli mcp serve
   ```

   Or, to share the config with your team, commit a `.mcp.json` at the project
   root instead:

   ```json
   {
     "mcpServers": {
       "orion": {
         "command": "orion-cli",
         "args": ["mcp", "serve"],
         "env": { "ORION_SERVER_URL": "http://localhost:8080" }
       }
     }
   }
   ```

4. **Verify:** start `claude` and run `/mcp` — you should see `orion` listed
   with 46 tools.

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

- The full tool catalog and client configs (Claude Desktop, Cursor, HTTP
  transport): [MCP Server Setup](./mcp-setup.md)
- No MCP client available? The [Prompt Pack](../getting-started/prompt-pack.md)
  gives any LLM the same powers over the plain REST API.
- Ready-made prompts for common services (webhook transforms, enrichment,
  routing): [Use Cases & Patterns](./use-cases.md)
