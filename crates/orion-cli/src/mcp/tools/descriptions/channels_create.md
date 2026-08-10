Create a new channel in the Orion engine. Channels are service endpoints that receive data and route it to a workflow for processing.

Channels are created in draft status. Activate them with `channels_activate`, then call `engine_reload` to load changes.

## Channel JSON Structure

Required fields:
- `name` (string) — unique channel name
- `channel_type` (string) — "sync" (blocking response) or "async" (returns trace ID)
- `protocol` (string) — "http", "rest", or "kafka"
- `workflow_id` (string) — ID of the workflow to execute

Optional fields:
- `description` (string) — human-readable description
- `methods` (array) — HTTP methods, e.g. ["GET", "POST"]
- `route_pattern` (string) — REST route pattern with path params, e.g. "/orders/{id}/details"
- `topic` (string) — Kafka topic name (for kafka protocol)
- `consumer_group` (string) — Kafka consumer group
- `priority` (integer) — route matching priority, higher = matched first
- `config` (object) — per-channel configuration:
  - `rate_limit` — `{"rps": 100, "burst": 50}`
  - `timeout_ms` — processing timeout in milliseconds
  - `cors` — `{"origins": ["*"]}`
  - `backpressure` — `{"max_concurrent": 100}`
  - `input_validation` — JSONLogic expression for request validation

## Example

```json
{
  "name": "orders-api",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["GET", "POST"],
  "route_pattern": "/orders/{order_id}",
  "workflow_id": "process-orders",
  "description": "REST API for order processing",
  "config": {
    "timeout_ms": 5000,
    "rate_limit": {"rps": 100, "burst": 50}
  }
}
```
