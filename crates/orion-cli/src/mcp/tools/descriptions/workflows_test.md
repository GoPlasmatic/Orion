Test/dry-run a workflow with sample data. Returns whether the workflow matched, the output data, and an execution trace showing each task's result.

The `data` parameter is the JSON payload that would normally arrive on the channel. For workflows that start with a `parse_json` task (source: "payload"), provide the raw business data directly:

```json
{"id": "order-123", "amount": 250.00, "customer": "acme"}
```

The trace output shows each task's execution result, making it easy to debug pipeline behavior.
