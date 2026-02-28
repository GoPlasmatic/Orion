# Configuration

[← Back to README](../README.md)

## Config File

```toml
[server]
host = "0.0.0.0"
port = 8080
# workers = <num_cpus>         # Defaults to number of CPU cores

[storage]
path = "orion.db"              # SQLite database file path
# max_connections = 5           # SQLite connection pool size

[ingest]
max_payload_size = 1048576     # Maximum payload size in bytes (1 MB)

[engine]
max_concurrent_workflows = 100

[queue]
workers = 4                    # Concurrent async job workers
buffer_size = 1000             # Channel buffer for pending jobs

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
```

## Environment Variable Overrides

Override any setting with environment variables using double-underscore nesting:

```bash
ORION_SERVER__PORT=9090
ORION_KAFKA__ENABLED=true
ORION_LOGGING__FORMAT=json
```

All settings have sensible defaults. You can run Orion with no config file at all — `orion-server` just works.

## Deployment

Orion ships as a **single binary** with an embedded SQLite database — no external dependencies to manage.

- **Standalone** — run directly on a VM or bare metal
- **Docker** — `docker build -t orion . && docker run -p 8080:8080 orion`
- **Sidecar** — deploy alongside your application in Kubernetes
- **Homebrew** — `brew install GoPlasmatic/tap/orion`

The embedded SQLite storage means you're up and running immediately. For production, configure persistent volume mounts for the database file.

## Graceful Shutdown

Orion handles `SIGTERM` and `SIGINT` with a controlled shutdown sequence:

1. HTTP server stops accepting new connections
2. In-flight requests complete
3. Kafka consumer (if enabled) is signaled to stop
4. Async job queue drains with a **30-second timeout**
5. Process exits

## Production Checklist

- Mount a persistent volume for `orion.db`
- Set `ORION_LOGGING__FORMAT=json` for structured log ingestion
- Enable Prometheus metrics with `ORION_METRICS__ENABLED=true`
- Tune `storage.max_connections` for your concurrency needs (default: 5)
- Use `RUST_LOG=orion=info` for per-crate log filtering
