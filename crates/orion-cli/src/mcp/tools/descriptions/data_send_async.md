Send data for asynchronous processing on a channel. Returns a trace ID immediately. Use `traces_get` to check the result, or `traces_list` to find traces by channel/status.

The data payload format is the same as `data_send_sync` — the raw business data JSON:

```json
{"id": "order-123", "amount": 250.00, "items": [{"sku": "A1", "qty": 2}]}
```

Use async processing for long-running rule pipelines or when you don't need the result immediately.
