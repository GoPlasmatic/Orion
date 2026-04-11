# Deployability

Orion ships as a single binary with embedded migrations, sensible defaults, and multiple installation methods. No runtime dependencies. You're up and running immediately.

## Packaging

Orion compiles into a **single binary** (`orion-server`) with everything built in:

- All three database backends (SQLite, PostgreSQL, MySQL) with embedded migrations
- Kafka producer and consumer
- OpenTelemetry trace export
- TLS termination
- Swagger UI at `/docs`

No feature flags, no plugins, no shared libraries to manage. The binary auto-detects the database backend from the URL scheme at startup.

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme | SQLite |
| Kafka | `kafka.enabled` | Disabled |
| OpenTelemetry | `tracing.enabled` | Disabled |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI | Always at `/docs` | Enabled |
| Metrics | `metrics.enabled` | Disabled |

With the default SQLite backend, there are zero external dependencies.

## Containerization

Orion uses a multi-stage Docker build for minimal image size:

```bash
docker build -t orion .
```

The Dockerfile uses `rust:1.93-slim` for building and `debian:trixie-slim` for the runtime image. Key characteristics:

- **Non-root execution:** the container runs as a non-root user
- **Built-in health probes:** `/healthz` (liveness) and `/readyz` (readiness) are available without additional configuration

**Docker with SQLite persistence:** SQLite stores data in a local file. Without a persistent volume, data is lost when the container restarts:

```bash
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

For production, PostgreSQL or MySQL is recommended. Point `ORION_STORAGE__URL` to your database server.

## Configuration

All settings have sensible defaults. You can run Orion with no config file at all. `orion-server` just works.

**TOML config file:** pass with `-c`:

```bash
orion-server -c config.toml
```

**Environment variable overrides:** use double-underscore nesting to override any setting:

```bash
ORION_SERVER__PORT=9090
ORION_STORAGE__URL="postgres://user:pass@localhost/orion"
ORION_KAFKA__ENABLED=true
ORION_LOGGING__FORMAT=json
ORION_ADMIN_AUTH__ENABLED=true
ORION_ADMIN_AUTH__API_KEY="your-secret-key"
```

Environment variables take precedence over the config file, making it easy to customize per environment without changing files.

**Runtime configuration:** channels carry their own runtime config (rate limits, timeouts, CORS, validation) via `config_json`, changeable through the admin API without restarts.

## Distribution

Orion is available through multiple installation methods:

**Homebrew** (macOS and Linux):

```bash
brew install GoPlasmatic/tap/orion-server
```

**Shell installer** (Linux/macOS):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
```

**PowerShell installer** (Windows):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.ps1 | iex"
```

**Docker:**

```bash
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest
```

**From source:**

```bash
cargo install --git https://github.com/GoPlasmatic/Orion
```

Multi-platform binaries are published for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).
