Orion MCP server — manage rules, connectors, data processing, execution traces, and engine operations on an Orion rules engine.

## Core Concepts

- **Rules** define processing logic: each rule has a condition (JSONLogic) and a task pipeline
- **Channels** group rules; data sent to a channel runs through all active rules on that channel
- **Tasks** are the steps inside a rule (parse_json → map → filter → http_call → publish_json, etc.)
- **Connectors** configure external service connections (HTTP APIs, Kafka) used by tasks
- **JSONLogic** is used for conditions and data transformations — see rules_create for syntax

## Typical Workflow

1. `health_check` — verify server is reachable
2. `connectors_create` — set up any external service connections needed
3. `rules_create` — define processing rules with task pipelines
4. `engine_reload` — **required** after creating/updating rules to activate changes
5. `rules_test` — dry-run a rule with sample data to verify behavior
6. `data_send_sync` / `data_send_async` — send real data for processing

## Important Notes

- Always call `engine_reload` after creating, updating, or deleting rules — changes are not active until reload
- Rules on the "default" channel process data sent to `/api/v1/data/default`
- Use `rules_test` to validate rules before activating them in production
