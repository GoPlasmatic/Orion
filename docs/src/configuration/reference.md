# Config Reference

All settings have sensible defaults. You can run Orion with no config file at all — `orion-server` just works.

## CLI Commands

```bash
orion-server                              # Start the server (default)
orion-server -c config.toml               # Start with a config file
orion-server validate-config              # Validate config without starting
orion-server validate-config -c config.toml  # Validate a specific config file
orion-server migrate                      # Run database migrations
orion-server migrate --dry-run            # Preview pending migrations
```

## Database Backend

The database backend is selected at runtime from the `storage.url` scheme — no rebuild needed:

| Backend | URL Format | Example |
|---------|------------|---------|
| **SQLite** | `sqlite:` | `sqlite:orion.db` or `sqlite::memory:` |
| **PostgreSQL** | `postgres://` | `postgres://user:pass@host/db` |
| **MySQL** | `mysql://` | `mysql://user:pass@host/db` |

```bash
# SQLite (default)
orion-server

# PostgreSQL
ORION_STORAGE__URL="postgres://user:pass@localhost/orion" orion-server

# MySQL
ORION_STORAGE__URL="mysql://user:pass@localhost/orion" orion-server
```

Migrations for all backends are embedded in the binary and the correct set is selected automatically at startup.

## Complete Config File

```toml
[server]
host = "0.0.0.0"
port = 8080
# shutdown_drain_secs = 30     # HTTP connection drain timeout on shutdown

# [server.tls]
# enabled = false
# cert_path = "cert.pem"
# key_path = "key.pem"

[storage]
url = "sqlite:orion.db"       # Database URL (sqlite:, postgres://, mysql://)
# max_connections = 25          # Connection pool max connections
# min_connections = 5           # Connection pool min connections
# busy_timeout_ms = 5000        # SQLite busy timeout (ignored for other backends)
# acquire_timeout_secs = 5      # Connection pool acquire timeout
# idle_timeout_secs = 0         # Connection idle timeout (0 = no timeout)

[ingest]
max_payload_size = 1048576     # Maximum payload size in bytes (1 MB)

[engine]
# health_check_timeout_secs = 2   # Timeout for engine read lock in health checks
# reload_timeout_secs = 10        # Timeout for engine write lock during reload
# max_channel_call_depth = 10     # Max recursion depth for channel_call
# default_channel_call_timeout_ms = 30000  # Default timeout for channel_call
# global_http_timeout_secs = 30   # Global timeout for HTTP client
# max_pool_cache_entries = 100    # Max cached connection pools for external connectors
# cache_cleanup_interval_secs = 60  # Cleanup interval for idle connection pools

# [engine.circuit_breaker]
# enabled = false                 # Enable circuit breakers for connectors
# failure_threshold = 5           # Failures before tripping the breaker
# recovery_timeout_secs = 30      # Cooldown before half-open probe
# max_breakers = 10000            # Max circuit breaker instances (LRU eviction)

[queue]
workers = 4                    # Concurrent async trace workers
buffer_size = 1000             # Channel buffer for pending traces
# shutdown_timeout_secs = 30    # Timeout for in-flight jobs during shutdown
# trace_retention_hours = 72    # How long to keep completed traces (0 = disabled)
# trace_cleanup_interval_secs = 3600  # Cleanup check interval
# processing_timeout_ms = 60000  # Per-trace processing timeout
# max_result_size_bytes = 1048576  # Max size of trace result (1 MB)
# max_queue_memory_bytes = 104857600  # Max memory for queued traces (100 MB)
# dlq_retry_enabled = true       # Enable dead letter queue retry
# dlq_max_retries = 5            # Max retry attempts for DLQ entries
# dlq_poll_interval_secs = 30    # DLQ retry poll interval

[rate_limit]
# enabled = false               # Enable platform-level rate limiting
# default_rps = 100             # Default requests per second
# default_burst = 50            # Default burst allowance

# [rate_limit.endpoints]
# admin_rps = 50                # Rate limit for admin routes
# data_rps = 200                # Rate limit for data routes

[admin_auth]
# enabled = false               # Require authentication for admin endpoints
# api_key = "your-secret-key"   # The API key or bearer token
# header = "Authorization"      # "Authorization" = Bearer format, other = raw key

[cors]
# allowed_origins = ["*"]       # Global CORS allowed origins

[kafka]
enabled = false
brokers = ["localhost:9092"]
group_id = "orion"
# processing_timeout_ms = 60000  # Per-message processing timeout
# max_inflight = 100             # Max in-flight messages
# lag_poll_interval_secs = 30    # Consumer lag poll interval

[[kafka.topics]]               # Map Kafka topics to channels
topic = "incoming-orders"
channel = "orders"

[kafka.dlq]
enabled = false
topic = "orion-dlq"

[logging]
level = "info"                 # trace, debug, info, warn, error
format = "pretty"              # pretty or json

[metrics]
enabled = false

[tracing]
# enabled = false                   # Enable OpenTelemetry trace export
# otlp_endpoint = "http://localhost:4317"  # OTLP gRPC endpoint
# service_name = "orion"            # Service name in traces
# sample_rate = 1.0                 # 0.0 (none) to 1.0 (all)

[channels]                          # Control which channels this instance loads
# include = ["orders.*", "payments.*"]   # Glob patterns to include (empty = all)
# exclude = ["analytics.*"]              # Glob patterns to exclude
```

## Environment Variable Overrides

Override any setting with environment variables using double-underscore nesting:

```bash
ORION_SERVER__PORT=9090
ORION_STORAGE__URL="postgres://user:pass@localhost/orion"
ORION_KAFKA__ENABLED=true
ORION_LOGGING__FORMAT=json
ORION_RATE_LIMIT__ENABLED=true
ORION_ADMIN_AUTH__ENABLED=true
ORION_ADMIN_AUTH__API_KEY="your-secret-key"
ORION_TRACING__ENABLED=true
ORION_TRACING__OTLP_ENDPOINT="http://jaeger:4317"
ORION_METRICS__ENABLED=true
```

Environment variables take precedence over config file values.

## Built-in Capabilities

All capabilities are compiled into a single binary and controlled at runtime:

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme | SQLite |
| Kafka | `kafka.enabled` | Disabled |
| OpenTelemetry | `tracing.enabled` | Disabled |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI | Always at `/docs` | Enabled |
| SQL connectors | `db_read`/`db_write` functions | Always available |
| Redis cache | `cache_read`/`cache_write` with Redis backend | Always available |
| MongoDB connector | `mongo_read` function | Always available |
| Rate limiting | `rate_limit.enabled` | Disabled |
| Metrics | `metrics.enabled` | Disabled |

## Production Checklist

- Mount a persistent volume for `orion.db` (SQLite) or configure `storage.url` for PostgreSQL/MySQL
- Enable admin API authentication: `ORION_ADMIN_AUTH__ENABLED=true`
- Set structured logging: `ORION_LOGGING__FORMAT=json`
- Enable Prometheus metrics: `ORION_METRICS__ENABLED=true`
- Configure rate limiting for traffic protection (platform-level and per-channel)
- Use per-crate filtering: `RUST_LOG=orion=info`
- Enable OpenTelemetry: `ORION_TRACING__ENABLED=true` for distributed tracing
- Enable TLS via the `server.tls` section
- Enable circuit breakers: `engine.circuit_breaker.enabled = true`
- Set trace retention: `queue.trace_retention_hours` for trace data lifecycle management
