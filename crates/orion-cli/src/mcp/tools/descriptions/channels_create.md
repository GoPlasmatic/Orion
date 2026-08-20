Create a new channel in the Orion engine. Channels are service endpoints that receive data and route it to a workflow for processing.

Channels are created in draft status. Activate them with `channels_activate` — activation hot-reloads the engine automatically; `engine_reload` is only needed after a change committed with `?reload=defer`, or to force a rebuild.

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
- `config` (object) — per-channel configuration. Unknown keys are **rejected**,
  so use these names exactly:
  - `rate_limit` — `{"requests_per_second": 100, "burst": 50}`. Optional
    `key_headers` (array) declares extra request headers `key_logic` may read,
    beyond `authorization`, `x-api-key`, `x-forwarded-for`, `x-real-ip`,
    `user-agent`, `content-type`, `origin`, `x-tenant-id`.
  - `timeout_ms` — processing timeout in milliseconds
  - `origin_allow_list` — `["https://app.example.com"]`, a server-side `Origin`
    check. Cross-origin *CORS* is instance config, not per channel.
  - `backpressure` — `{"max_concurrent_per_node": 100}`
  - `validation_logic` — JSONLogic expression for request validation
  - `deduplication`, `cache`, `response`, `auth`, `tracing`

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
