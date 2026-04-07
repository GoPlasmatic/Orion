# Configuration

[← Back to README](../README.md)

## Database Backend

Orion supports three database backends, selected at **compile time** via feature flags:

| Backend | Feature Flag | Default | URL Format |
|---------|-------------|---------|------------|
| **SQLite** | `db-sqlite` | Yes | `sqlite:orion.db` |
| **PostgreSQL** | `db-postgres` | No | `postgres://user:pass@host/db` |
| **MySQL** | `db-mysql` | No | `mysql://user:pass@host/db` |

Pick **one** backend per build:

```bash
cargo build                              # SQLite (default)
cargo build --no-default-features --features db-postgres,swagger-ui
cargo build --no-default-features --features db-mysql,swagger-ui
```

Migrations are embedded at compile time from `migrations/{sqlite,postgres,mysql}/` and run automatically on startup.

## Config File

```toml
[server]
host = "0.0.0.0"
port = 8080
# shutdown_drain_secs = 30     # HTTP connection drain timeout on shutdown

[storage]
url = "sqlite:orion.db"       # Database URL (sqlite:, postgres://, mysql://)
# max_connections = 10          # Connection pool max connections
# busy_timeout_ms = 5000        # SQLite busy timeout in milliseconds (ignored for other backends)
# acquire_timeout_secs = 5      # Connection pool acquire timeout in seconds

[ingest]
max_payload_size = 1048576     # Maximum payload size in bytes (1 MB)

[engine]
# health_check_timeout_secs = 2   # Timeout for engine read lock in health checks
# reload_timeout_secs = 10        # Timeout for engine write lock during reload
# max_channel_call_depth = 10     # Max recursion depth for channel_call
# default_channel_call_timeout_ms = 30000  # Default timeout for channel_call

[queue]
workers = 4                    # Concurrent async trace workers
buffer_size = 1000             # Channel buffer for pending traces
# shutdown_timeout_secs = 30    # Timeout for in-flight jobs during shutdown
# trace_retention_hours = 72    # How long to keep completed traces
# trace_cleanup_interval_secs = 3600  # Cleanup check interval

[rate_limit]
# enabled = false               # Enable platform-level request rate limiting
# default_rps = 100             # Default requests per second
# default_burst = 50            # Default burst allowance

# [rate_limit.endpoints]
# admin_rps = 50                # Rate limit for admin routes
# data_rps = 200                # Rate limit for data routes

# Per-channel rate limits are configured via the channel's config_json in the DB.

[admin_auth]
# enabled = false               # Require authentication for admin endpoints
# api_key = "your-secret-key"   # The API key or bearer token
# header = "Authorization"      # "Authorization" = Bearer format, other = raw key

[cors]
# allowed_origins = ["*"]       # Global CORS allowed origins

[kafka]                        # Requires the `kafka` feature flag
enabled = false
brokers = ["localhost:9092"]
group_id = "orion"

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

[tracing]                           # Requires the `otel` feature flag
# enabled = false                   # Enable OpenTelemetry trace export
# otlp_endpoint = "http://localhost:4317"  # OTLP gRPC endpoint
# service_name = "orion"            # Service name in traces
# sample_rate = 1.0                 # 0.0 (none) to 1.0 (all)

# [channels]                        # Control which channels this instance loads
# include = ["orders.*", "payments.*"]   # Glob patterns to include
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
```

All settings have sensible defaults. You can run Orion with no config file at all — `orion-server` just works.

## Deployment

Orion ships as a **single binary**. With the default SQLite backend, there are no external dependencies — you're up and running immediately.

- **Standalone** — run directly on a VM or bare metal
- **Docker** — `docker build -t orion . && docker run -p 8080:8080 orion`
- **Sidecar** — deploy alongside your application in Kubernetes
- **Homebrew** — `brew install GoPlasmatic/tap/orion`

For production with SQLite, configure persistent volume mounts for the database file (including `.wal` and `.shm` sidecar files). For PostgreSQL or MySQL, point `storage.url` to your database server.

## Graceful Shutdown

Orion handles `SIGTERM` and `SIGINT` with a controlled shutdown sequence:

1. HTTP server stops accepting new connections
2. In-flight requests drain (configurable via `shutdown_drain_secs`, default 30s)
3. Kafka consumer (if enabled) is signaled to stop
4. Trace cleanup task is stopped
5. Async trace queue drains with timeout
6. OpenTelemetry spans are flushed (if enabled)
7. Process exits

## Production Checklist

- Mount a persistent volume for `orion.db` (SQLite) or configure `storage.url` for PostgreSQL/MySQL
- Enable admin API authentication with `ORION_ADMIN_AUTH__ENABLED=true`
- Set `ORION_LOGGING__FORMAT=json` for structured log ingestion
- Enable Prometheus metrics with `ORION_METRICS__ENABLED=true`
- Configure `rate_limit` for traffic protection (platform-level and per-channel)
- Use `RUST_LOG=orion=info` for per-crate log filtering
- Enable OpenTelemetry with `ORION_TRACING__ENABLED=true` for distributed tracing
- Enable TLS with `--features tls` and configure `server.tls` section
