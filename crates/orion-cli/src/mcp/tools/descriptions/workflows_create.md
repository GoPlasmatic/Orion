Create a new workflow in the Orion engine. Workflows define processing pipelines (task sequences) that execute when data arrives on a linked channel.

Workflows are created in draft status. Activate them with `workflows_activate`, then call `engine_reload` to load changes.

## Workflow JSON Structure

Required fields:
- `name` (string) — unique workflow name
- `tasks` (array) — one or more task objects (the processing pipeline)

Optional fields:
- `workflow_id` (string) — custom human-readable ID (auto-generated if omitted)
- `priority` (integer) — execution order, higher = runs first, default: 0
- `condition` (JSONLogic | boolean) — when to run this workflow, default: true (always)
- `description` (string) — human-readable description
- `tags` (array of strings) — for filtering/organization
- `continue_on_error` (boolean) — continue pipeline if a task fails, default: false

## Task Structure

Each task in the `tasks` array:
```json
{
  "id": "unique_task_id",
  "name": "Human-readable name",
  "function": {
    "name": "function_type",
    "input": { ... }
  },
  "condition": <optional JSONLogic>
}
```

## Built-in Task Functions

### parse_json — Parse raw input payload into a named variable
```json
{"name": "parse_json", "input": {"source": "payload", "target": "order"}}
```

### map — Transform data using JSONLogic expressions
```json
{"name": "map", "input": {"mappings": [
  {"path": "data.total_with_tax", "logic": {"*": [{"var": "data.order.total"}, 1.1]}}
]}}
```

### filter — Continue pipeline only if condition is true
```json
{"name": "filter", "input": {"condition": {">": [{"var": "data.order.amount"}, 100]}}}
```

### validation — Validate data using JSONLogic rules
```json
{"name": "validation", "input": {
  "rules": [
    {"logic": {"!!": [{"var": "data.order.id"}]}, "message": "Order ID is required"}
  ]
}}
```

### http_call — Call an external HTTP API via a connector
```json
{"name": "http_call", "input": {
  "connector": "my_api",
  "method": "POST",
  "path": "/api/orders",
  "body_logic": {"var": "data.order"},
  "response_path": "data.api_response"
}}
```

### channel_call — Invoke another channel's workflow in-process
```json
{"name": "channel_call", "input": {
  "channel": "auth-service",
  "data": {"user_id": "123"},
  "response_path": "auth_result"
}}
```

### log — Emit a structured log entry
```json
{"name": "log", "input": {"level": "info", "message": {"cat": ["Processing ", {"var": "data.order.id"}]}}}
```

### publish_json / publish_xml — Serialize data to string output
```json
{"name": "publish_json", "input": {"source": "order", "target": "json_string"}}
```

### publish_kafka — Publish to a Kafka topic
```json
{"name": "publish_kafka", "input": {"connector": "kafka_prod", "topic": "processed-orders"}}
```

## Complete Example

```json
{
  "name": "process_high_value_orders",
  "workflow_id": "high-value-orders",
  "priority": 10,
  "description": "Process orders over $100 with tax calculation",
  "tags": ["orders", "high-value"],
  "tasks": [
    {
      "id": "parse",
      "name": "Parse input",
      "function": {"name": "parse_json", "input": {"source": "payload", "target": "order"}}
    },
    {
      "id": "validate",
      "name": "Validate required fields",
      "function": {"name": "validation", "input": {
        "rules": [
          {"logic": {"!!": [{"var": "data.order.id"}]}, "message": "Order ID is required"},
          {"logic": {">": [{"var": "data.order.amount"}, 0]}, "message": "Amount must be positive"}
        ]
      }}
    },
    {
      "id": "calc_tax",
      "name": "Calculate tax",
      "function": {"name": "map", "input": {"mappings": [
        {"path": "data.order.total_with_tax", "logic": {"*": [{"var": "data.order.amount"}, 1.1]}}
      ]}}
    }
  ]
}
```
