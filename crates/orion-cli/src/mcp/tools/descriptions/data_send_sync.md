Send data for synchronous processing on a channel. Returns the processed result immediately.

The data payload is the business data that rules on the channel will process. For rules starting with a `parse_json` task, this is the raw JSON object:

```json
{"id": "order-123", "amount": 250.00, "items": [{"sku": "A1", "qty": 2}]}
```

The response contains the output produced by the rule pipeline (e.g., from publish_json tasks).
