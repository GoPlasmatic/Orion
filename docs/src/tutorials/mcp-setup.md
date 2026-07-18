# MCP Server Setup

Orion includes an MCP (Model Context Protocol) server that lets AI assistants like Claude manage workflows, channels, and connectors through natural language. The MCP server wraps the Orion admin API, giving AI agents full control over your Orion instance.

<div class="asciinema-player" data-cast="casts/mcp.cast"></div>
<span class="asciinema-caption">▶ Click to play. A real stdio JSON-RPC session: handshake, tool discovery, then a live tool call.</span>

## Prerequisites

- A running Orion instance (see [Install & First Service](./cli-setup.md))
- An MCP-compatible client (Claude Code, Claude Desktop, or other MCP clients)

> No MCP client? Use the [Prompt Pack](../getting-started/prompt-pack.md) — a
> self-contained context block that lets any LLM drive Orion through the plain
> REST API.

## Configuration

Add the Orion MCP server to your MCP client configuration.

**Claude Code:** add to your `.claude/settings.json` or project-level `.mcp.json`:

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

**Claude Desktop:** add to your Claude Desktop configuration file:

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

If admin authentication is enabled on your Orion instance, include the API key:

```json
{
  "env": {
    "ORION_SERVER_URL": "http://localhost:8080",
    "ORION_API_KEY": "your-secret-key"
  }
}
```

## Available Tools

The MCP server exposes **46 tools** covering the full Orion API. Tool names follow a
`<resource>_<action>` convention:

| Category | Tools |
|----------|-------|
| **Health** | `health_check` |
| **Engine** | `engine_status`, `engine_reload` |
| **Workflows** | `workflows_list`, `workflows_get`, `workflows_create`, `workflows_update`, `workflows_delete`, `workflows_activate`, `workflows_archive`, `workflows_test`, `workflows_validate`, `workflows_rollout`, `workflows_versions`, `workflows_create_version`, `workflows_export`, `workflows_import` |
| **Channels** | `channels_list`, `channels_get`, `channels_create`, `channels_update`, `channels_delete`, `channels_activate`, `channels_archive`, `channels_versions`, `channels_create_version`, `channels_import` |
| **Connectors** | `connectors_list`, `connectors_get`, `connectors_create`, `connectors_update`, `connectors_delete`, `connectors_enable`, `connectors_disable`, `connectors_import` |
| **Circuit Breakers** | `circuit_breakers_list`, `circuit_breaker_reset` |
| **Data** | `data_send_sync`, `data_send_async` |
| **Traces** | `traces_list`, `traces_get` |
| **Functions** | `functions_list` |
| **Audit Logs** | `audit_logs_list` |
| **Backups** | `backups_create`, `backups_list` |
| **Metrics** | `get_metrics` |

## Usage Example

Once configured, you can use natural language to manage Orion:

> "Create a workflow that parses incoming orders, flags any over $10,000, and adds a risk level field. Then create a channel called 'orders' that uses it."

The AI assistant will use the MCP tools to:

1. Create the workflow via `workflows_create`
2. Test it with sample data via `workflows_test`
3. Activate it via `workflows_activate`
4. Create the channel via `channels_create`
5. Activate the channel via `channels_activate`, then apply with `engine_reload`

## Troubleshooting

**MCP server not connecting:**
- Verify Orion is running: `curl http://localhost:8080/health`
- Check the `ORION_SERVER_URL` environment variable is set correctly
- Ensure `orion-cli` is in your PATH

**Authentication errors:**
- Verify `ORION_API_KEY` matches your Orion instance's `admin_auth.api_key`
- Check that admin auth is enabled on the Orion instance if you're passing a key
