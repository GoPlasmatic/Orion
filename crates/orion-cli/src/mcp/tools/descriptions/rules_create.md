Create a new rule in the Orion engine. Rules define processing pipelines that execute when data arrives on a channel.

After creating a rule, call `engine_reload` to activate it.

## Rule JSON Structure

Required fields:
- `name` (string) — unique rule name
- `tasks` (array) — one or more task objects (the processing pipeline)

Optional fields:
- `channel` (string) — channel name, default: "default"
- `priority` (integer) — execution order, higher = runs first, default: 0
- `condition` (JSONLogic | boolean) — when to run this rule, default: true (always)
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
`source`: where to read from ("payload", "payload.body", "data.field"). `target`: field name to store under data (result at `data.{target}`).
**Start most rules with this task** to make the raw input accessible as a named object.

### parse_xml — Parse XML payload into structured JSON
```json
{"name": "parse_xml", "input": {"source": "payload", "target": "document"}}
```
Same `source`/`target` as parse_json. The XML string is parsed into a JSON object.

### map — Transform data using JSONLogic expressions
```json
{"name": "map", "input": {"mappings": [
  {"path": "data.total_with_tax", "logic": {"*": [{"var": "data.order.total"}, 1.1]}},
  {"path": "data.status", "logic": "processed"},
  {"path": "metadata.processed", "logic": true}
]}}
```
Each mapping has `path` (dot-notation target, e.g. "data.x", "metadata.y", "temp_data.z") and `logic` (JSONLogic expression or static value). Null results are skipped. Root fields (data, metadata, temp_data) are merged, not replaced.

### filter — Continue pipeline only if condition is true
```json
{"name": "filter", "input": {"condition": {">": [{"var": "data.order.amount"}, 100]}}}
```
Optional `on_reject`: `"halt"` (default) stops the entire pipeline, `"skip"` skips only this task and continues.

### validation — Validate data using JSONLogic rules
```json
{"name": "validation", "input": {
  "rules": [
    {"logic": {"!!": [{"var": "data.order.id"}]}, "message": "Order ID is required"},
    {"logic": {">": [{"var": "data.order.amount"}, 0]}, "message": "Amount must be positive"}
  ]
}}
```
Also accepts function name `"validate"`. Each rule has `logic` (JSONLogic that must evaluate to exactly `true`) and `message` (error shown on failure, defaults to "Validation failed"). Validation errors are collected; the task returns status 400 if any rule fails.

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
Required: `connector` (connector name). Optional fields:
- `method`: GET (default), POST, PUT, PATCH, DELETE
- `path`: static URL path appended to connector's base URL
- `path_logic`: JSONLogic expression for dynamic path (alternative to `path`)
- `body`: static JSON request body
- `body_logic`: JSONLogic expression for dynamic body (alternative to `body`)
- `headers`: static task-level headers as key-value pairs (merged with connector headers)
- `headers_logic`: JSONLogic expression for dynamic headers (alternative to `headers`)
- `response_path`: dot-path where the response is stored in context
- `timeout_ms`: request timeout in ms (default: 30000)

Use `http_call` with `response_path` to fetch and merge external data into the pipeline (e.g., enrichment scenarios).

### log — Emit a structured log entry
```json
{"name": "log", "input": {
  "level": "info",
  "message": {"cat": ["Processing order ", {"var": "data.order.id"}]},
  "fields": {"order_amount": {"var": "data.order.amount"}}
}}
```
`message` is a JSONLogic expression (can be a plain string or dynamic). `level`: trace, debug, info (default), warn, error. `fields`: optional map of key → JSONLogic expressions for structured logging.

### publish_json — Serialize data to JSON string output
```json
{"name": "publish_json", "input": {"source": "order", "target": "json_string"}}
```
Required: `source` (field name in data to serialize, e.g. "order" reads `data.order`; supports nested paths like "response.body"), `target` (field name in data where the JSON string is stored at `data.{target}`). Optional: `pretty` (boolean, default: false).

### publish_xml — Serialize data to XML string output
```json
{"name": "publish_xml", "input": {"source": "order", "target": "xml_string", "root_element": "order"}}
```
Same `source`/`target` as publish_json. Optional: `root_element` (XML root tag name, default: "root"), `pretty` (boolean, default: false).

### publish_kafka — Publish to a Kafka topic via a connector
```json
{"name": "publish_kafka", "input": {
  "connector": "kafka_prod",
  "topic": "processed-orders",
  "key_logic": {"var": "data.order.id"},
  "value_logic": {"var": "data.order"}
}}
```
Required: `connector` (Kafka connector name), `topic` (Kafka topic). Optional: `key_logic` (JSONLogic for message key), `value_logic` (JSONLogic for message value; defaults to entire message data as JSON).

## JSONLogic Quick Reference

- Variable access: `{"var": "data.order.amount"}`
- Comparison: `{">":[{"var":"data.amount"},100]}`, `{"==": [{"var":"data.status"}, "active"]}`
- Logic: `{"and": [cond1, cond2]}`, `{"or": [cond1, cond2]}`, `{"!": cond}`
- Membership: `{"in": [{"var":"data.type"}, ["A","B","C"]]}`
- String concat: `{"cat": ["Hello ", {"var": "data.name"}]}`
- Conditional: `{"if": [cond, then_value, else_value]}`
- Arithmetic: `{"+": [1, 2]}`, `{"*": [{"var":"data.price"}, {"var":"data.qty"}]}`
- Current time: `{"now": []}`

## Complete Example

```json
{
  "name": "process_high_value_orders",
  "channel": "orders",
  "priority": 10,
  "description": "Process orders over $100 with tax calculation",
  "tags": ["orders", "high-value"],
  "condition": true,
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
      "id": "filter_high_value",
      "name": "Only high-value orders",
      "function": {"name": "filter", "input": {
        "condition": {">": [{"var": "data.order.amount"}, 100]}
      }}
    },
    {
      "id": "calc_tax",
      "name": "Calculate tax",
      "function": {"name": "map", "input": {"mappings": [
        {"path": "data.order.total_with_tax", "logic": {"*": [{"var": "data.order.amount"}, 1.1]}}
      ]}}
    },
    {
      "id": "output",
      "name": "Publish result",
      "function": {"name": "publish_json", "input": {"source": "order", "target": "json_string"}}
    }
  ]
}
```
