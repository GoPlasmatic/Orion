# MCP Server Setup

`orion-cli` carries an MCP (Model Context Protocol) server that exposes Orion's
admin API as tools. An MCP-capable assistant can then create workflows, dry-run
them, activate channels, and read traces by being asked to — no prompt
engineering, no hand-written HTTP.

<div class="asciinema-player" data-cast="casts/mcp.cast"></div>
<span class="asciinema-caption">▶ Click to play. A real stdio JSON-RPC session: handshake, tool discovery, then a live tool call.</span>

## Before you start

- A running Orion instance — see [Install & Run](../getting-started/install.md).
- `orion-cli` on your `PATH`, from the same page.
- An MCP-capable client: Claude Code, Claude Desktop, Cursor, or any other.

> [!TIP]
> No MCP client? The [Prompt Pack](./prompt-pack.md) is a self-contained context
> block that lets any LLM drive Orion through the plain REST API.

## Configure your client

The server runs over stdio. Every stdio client takes the same three facts —
the command, its arguments, and the environment that points it at your instance:

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion-cli",
      "args": ["mcp", "serve"],
      "env": {
        "ORION_SERVER_URL": "http://localhost:8080"
      }
    }
  }
}
```

- **Claude Code:** `.claude/settings.json`, or a project-level `.mcp.json` you
  can commit for your team. `claude mcp add` writes it for you — see
  [Build a Service with Claude Code](./claude-code.md).
- **Claude Desktop:** the same block, in
  `~/Library/Application Support/Claude/claude_desktop_config.json`.
- **Cursor:** Settings → MCP Servers takes the inner object only:

  ```json
  {
    "orion": {
      "command": "orion-cli",
      "args": ["mcp", "serve"],
      "env": { "ORION_SERVER_URL": "http://localhost:8080" }
    }
  }
  ```

If admin authentication is enabled on the instance, add the key to the same
`env` block:

```json
{ "env": { "ORION_SERVER_URL": "http://localhost:8080", "ORION_API_KEY": "your-secret-key" } }
```

`ORION_API_KEY_HEADER` overrides the header the key is sent in. Without an `env`
block, `orion-cli` falls back to whatever `~/.orion/config.toml` holds.

## Remote clients: HTTP transport

For a client that cannot spawn a local process, run the MCP server as a network
service instead:

```bash
orion-cli mcp serve --http                    # binds 0.0.0.0:8081
orion-cli mcp serve --http --bind 127.0.0.1:9000
```

The endpoint is `/mcp` on that address — `http://localhost:8081/mcp` by default.
Point your client's HTTP/streamable transport at that URL; the exact
configuration key is client-specific, so use your client's documentation for the
JSON shape.

> [!WARNING]
> The HTTP transport has no authentication of its own. Anything that can reach
> the port gets your instance's admin surface. Bind it to loopback, or put it
> behind a proxy that authenticates.

## What the assistant can do

The tools cover the full admin API, named `<resource>_<action>`:

| Category | What it covers |
|----------|----------------|
| **Workflows** | create, update, version, test, validate, activate, archive, roll out, dependencies, delete, import/export |
| **Channels** | create, update, version, activate, archive, delete, import |
| **Connectors** | create, update, enable/disable, test, delete, import |
| **Data** | send a request to a channel, sync or async |
| **Traces** | list and read execution traces |
| **Trace DLQ** | list and read dead-lettered submissions, and requeue one |
| **Packages** | list and read promotion receipts |
| **Engine** | status and reload |
| **Operations** | health, metrics, audit logs, circuit breakers, backups |
| **Discovery** | list the built-in functions with their input schemas |

The authoritative list is the one your client shows after connecting — it is
generated from the running CLI, so it never drifts from what is installed.

## Verify it

Ask the assistant for something read-only first:

> Is my Orion instance healthy? How many workflows and channels are active?

A correct answer means the handshake, the tool listing, and a live call all
worked. From there, [Build a Service with Claude Code](./claude-code.md) walks a
full session.

## Troubleshooting

**The server does not connect.** Check that Orion answers
`curl http://localhost:8080/health`, that `orion-cli` is on the `PATH` the
client uses (GUI apps often have a different one), and that `ORION_SERVER_URL`
has no trailing slash.

**Every call returns an authentication error.** `ORION_API_KEY` must match one of
the instance's `admin_auth.api_keys`. If admin auth is off, remove the key
rather than sending an unused one.

## Related

- [Build a Service with Claude Code](./claude-code.md) — a full guided session
  using these tools.
- [Prompt Pack (any LLM)](./prompt-pack.md) — the same job without an MCP
  client.
- [CLI Reference](../reference/cli.md) — every `orion-cli` command, including
  `mcp serve`.
