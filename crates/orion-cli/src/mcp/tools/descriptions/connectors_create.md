Create a new connector for external service integration. Connectors are referenced by name in rule tasks (http_call, publish_kafka).

## HTTP Connector

```json
{
  "name": "my_api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.example.com",
    "method": "POST",
    "timeout_ms": 30000,
    "headers": {
      "Content-Type": "application/json"
    },
    "auth": {
      "type": "bearer",
      "token": "your-api-token"
    },
    "retry": {
      "max_retries": 3,
      "retry_delay_ms": 1000
    },
    "max_response_size": 10485760
  }
}
```

Config fields:
- `type` (required): must be `"http"`
- `url` (required): base URL (http or https)
- `method` (optional): default HTTP method (GET, POST, etc.)
- `headers` (optional): default headers as key-value pairs
- `auth` (optional): authentication config (see below)
- `retry` (optional): `max_retries` (default: 3), `retry_delay_ms` (default: 1000)
- `max_response_size` (optional): max response bytes, default: 10485760 (10 MB)

### Auth types:
- **Bearer**: `{"type": "bearer", "token": "..."}`
- **Basic**: `{"type": "basic", "username": "...", "password": "..."}`
- **API Key**: `{"type": "apikey", "header": "X-API-Key", "key": "..."}`
- **None**: omit the `auth` field entirely

## Kafka Connector

```json
{
  "name": "kafka_prod",
  "connector_type": "kafka",
  "config": {
    "type": "kafka",
    "brokers": ["localhost:9092"],
    "topic": "events",
    "group_id": "orion-consumer"
  }
}
```

Config fields:
- `type` (required): must be `"kafka"`
- `brokers` (required): array of broker addresses
- `topic` (required): Kafka topic name
- `group_id` (optional): consumer group ID
