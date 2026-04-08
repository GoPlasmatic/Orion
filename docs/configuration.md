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

# [server.tls]                  # Requires the `tls` feature flag
# enabled = false
# cert_path = "cert.pem"
# key_path = "key.pem"

[storage]
url = "sqlite:orion.db"       # Database URL (sqlite:, postgres://, mysql://)
# max_connections = 25          # Connection pool max connections
# min_connections = 5           # Connection pool min connections
# busy_timeout_ms = 5000        # SQLite busy timeout in milliseconds (ignored for other backends)
# acquire_timeout_secs = 5      # Connection pool acquire timeout in seconds
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

[tracing]                           # Requires the `otel` feature flag
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
```

All settings have sensible defaults. You can run Orion with no config file at all — `orion-server` just works.

## CLI Commands

```bash
orion-server                          # Start the server (default)
orion-server -c config.toml           # Start with a config file
orion-server validate-config          # Validate config without starting
orion-server validate-config -c config.toml  # Validate a specific config file
orion-server migrate                  # Run database migrations
orion-server migrate --dry-run        # Preview pending migrations
```

## Feature Flags

| Flag | Default | Description |
|------|---------|-------------|
| `db-sqlite` | Yes | SQLite storage backend |
| `db-postgres` | No | PostgreSQL storage backend |
| `db-mysql` | No | MySQL storage backend |
| `kafka` | Yes | Kafka producer & consumer support |
| `otel` | Yes | OpenTelemetry trace export (OTLP) |
| `swagger-ui` | Yes | Swagger UI at `/docs` |
| `tls` | No | HTTPS/TLS support via rustls |
| `connectors-sql` | No | External SQL connectors (`db_read`, `db_write`) |
| `connectors-redis` | No | External Redis connectors (Redis-backed cache) |
| `connectors-mongodb` | No | External MongoDB connectors (`mongo_read`) |

Build with specific features:

```bash
cargo build --features kafka,otel,connectors-sql
cargo build --no-default-features --features db-postgres,swagger-ui,tls
```

## Deployment

Orion ships as a **single binary**. With the default SQLite backend, there are no external dependencies — you're up and running immediately.

- **Standalone** — run directly on a VM or bare metal
- **Docker** — `docker build -t orion . && docker run -p 8080:8080 orion`
- **Sidecar** — deploy alongside your application in Kubernetes
- **Homebrew** — `brew install GoPlasmatic/tap/orion`

For PostgreSQL or MySQL, point `storage.url` to your database server.

### Docker with SQLite Persistence

SQLite stores data in a local file. Without a persistent volume, **data is lost when the container restarts**. SQLite WAL mode also creates `.wal` and `.shm` sidecar files that must be on the same volume as the main database file.

```bash
# Named volume (recommended)
docker run -p 8080:8080 \
  -v orion-data:/app/data \
  -e ORION_STORAGE__URL=sqlite:/app/data/orion.db \
  orion
```

With Docker Compose:

```yaml
services:
  orion:
    image: orion
    ports:
      - "8080:8080"
    environment:
      ORION_STORAGE__URL: sqlite:/app/data/orion.db
    volumes:
      - orion-data:/app/data

volumes:
  orion-data:
```

### Docker with Custom Features

```bash
# PostgreSQL + Kafka + OTEL
docker build -t orion --build-arg FEATURES="db-postgres,kafka,otel" .

# Minimal SQLite-only build
docker build -t orion --build-arg FEATURES="db-sqlite,swagger-ui" .
```

## Graceful Shutdown

Orion handles `SIGTERM` and `SIGINT` with a controlled shutdown sequence:

1. HTTP server stops accepting new connections
2. In-flight requests drain (configurable via `shutdown_drain_secs`, default 30s)
3. Kafka consumer (if enabled) is signaled to stop
4. Trace cleanup task is stopped
5. DLQ retry consumer is stopped
6. Async trace queue drains with timeout
7. OpenTelemetry spans are flushed (if enabled)
8. Process exits

## Production Checklist

- Mount a persistent volume for `orion.db` (SQLite) or configure `storage.url` for PostgreSQL/MySQL
- Enable admin API authentication with `ORION_ADMIN_AUTH__ENABLED=true`
- Set `ORION_LOGGING__FORMAT=json` for structured log ingestion
- Enable Prometheus metrics with `ORION_METRICS__ENABLED=true`
- Configure `rate_limit` for traffic protection (platform-level and per-channel)
- Use `RUST_LOG=orion=info` for per-crate log filtering
- Enable OpenTelemetry with `ORION_TRACING__ENABLED=true` for distributed tracing
- Enable TLS with `--features tls` and configure `server.tls` section
- Enable circuit breakers with `engine.circuit_breaker.enabled = true`
- Set `queue.trace_retention_hours` for trace data lifecycle management
