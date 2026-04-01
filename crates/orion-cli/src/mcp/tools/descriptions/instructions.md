Orion MCP server — manage workflows, channels, connectors, data processing, execution traces, and engine operations on an Orion services runtime.

## Core Concepts

- **Workflows** define processing logic: each workflow has a condition (JSONLogic) and a task pipeline
- **Channels** are service endpoints that receive data and route it to a workflow for processing
- **Tasks** are the steps inside a workflow (parse_json -> map -> filter -> http_call -> channel_call -> publish_json, etc.)
- **Connectors** configure external service connections (HTTP APIs, Kafka) used by tasks
- **JSONLogic** is used for conditions and data transformations — see workflows_create for syntax

## Typical Workflow

1. `health_check` — verify server is reachable
2. `connectors_create` — set up any external service connections needed
3. `workflows_create` — define processing workflows with task pipelines
4. `channels_create` — create channels (service endpoints) that link to workflows
5. `workflows_activate` / `channels_activate` — activate the draft entities
6. `engine_reload` — **required** after creating/updating workflows and channels to activate changes
7. `workflows_test` — dry-run a workflow with sample data to verify behavior
8. `data_send_sync` / `data_send_async` — send real data for processing

## Important Notes

- Always call `engine_reload` after creating, updating, or deleting workflows/channels — changes are not active until reload
- Workflows and channels are created in "draft" status — activate them before they can process data
- Channels define how data enters the system (REST routes, HTTP endpoints, Kafka topics)
- Workflows define what happens to the data (the processing pipeline)
- Use `workflows_test` to validate workflows before activating them in production
