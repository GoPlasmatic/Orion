# MCP Server Setup

Orion includes an MCP (Model Context Protocol) server that lets AI assistants like Claude manage workflows, channels, and connectors through natural language. The MCP server wraps the Orion admin API, giving AI agents full control over your Orion instance.

## Prerequisites

- A running Orion instance (see [CLI Setup](./cli-setup.md))
- An MCP-compatible client (Claude Code, Claude Desktop, or other MCP clients)

## Configuration

Add the Orion MCP server to your MCP client configuration.

**Claude Code** — add to your `.claude/settings.json` or project-level `.mcp.json`:

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion-server",
      "args": ["mcp"],
      "env": {
        "ORION_URL": "http://localhost:8080"
      }
    }
  }
}
```

**Claude Desktop** — add to your Claude Desktop configuration file:

```json
{
  "mcpServers": {
    "orion": {
      "command": "orion-server",
      "args": ["mcp"],
      "env": {
        "ORION_URL": "http://localhost:8080"
      }
    }
  }
}
```

If admin authentication is enabled on your Orion instance, include the API key:

```json
{
  "env": {
    "ORION_URL": "http://localhost:8080",
    "ORION_API_KEY": "your-secret-key"
  }
}
```

## Available Tools

The MCP server exposes the following tools to AI assistants:

| Tool | Description |
|------|-------------|
| `list_workflows` | List all workflows with optional status/tag filters |
| `create_workflow` | Create a new workflow (as draft) |
| `get_workflow` | Get workflow details by ID |
| `update_workflow` | Update a draft workflow |
| `activate_workflow` | Activate a draft workflow |
| `archive_workflow` | Archive a workflow |
| `test_workflow` | Dry-run a workflow with sample data |
| `validate_workflow` | Validate workflow structure |
| `list_channels` | List all channels |
| `create_channel` | Create a new channel |
| `activate_channel` | Activate a draft channel |
| `list_connectors` | List connectors (secrets masked) |
| `create_connector` | Create a new connector |
| `engine_status` | Get engine status |
| `reload_engine` | Hot-reload the engine |
| `health_check` | Check Orion health |

## Usage Example

Once configured, you can use natural language to manage Orion:

> "Create a workflow that parses incoming orders, flags any over $10,000, and adds a risk level field. Then create a channel called 'orders' that uses it."

The AI assistant will use the MCP tools to:

1. Create the workflow via `create_workflow`
2. Test it with sample data via `test_workflow`
3. Activate it via `activate_workflow`
4. Create the channel via `create_channel`
5. Activate the channel via `activate_channel`

## Troubleshooting

**MCP server not connecting:**
- Verify Orion is running: `curl http://localhost:8080/health`
- Check the `ORION_URL` environment variable is set correctly
- Ensure `orion-server` is in your PATH

**Authentication errors:**
- Verify `ORION_API_KEY` matches your Orion instance's `admin_auth.api_key`
- Check that admin auth is enabled on the Orion instance if you're passing a key
