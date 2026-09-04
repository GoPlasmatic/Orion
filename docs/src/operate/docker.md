<!-- description: Run Orion as a container: one image with all three database backends, Kafka, OTLP and TLS compiled in, plus compose files for single-node and HA topologies. -->
# Deploy with Docker

Orion ships as a container image with everything compiled in — all three
database backends, Kafka, OTLP export, TLS. There is nothing to install
alongside it.

```bash
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest
```

That command gets you a working instance. The rest of this page is what to
change before it holds anything you care about.

## Give SQLite a volume

> [!WARNING]
> **The default SQLite database lives inside the container.** Without a volume,
> every workflow, channel, and connector you create is lost when the container
> is replaced, which includes every image upgrade.

```bash
docker run -p 8080:8080 \
  -v orion-data:/app/data \
  -e ORION_STORAGE__URL=sqlite:/app/data/orion.db \
  ghcr.io/goplasmatic/orion:latest
```

Both halves are needed: the volume gives the file somewhere durable to live, and
`ORION_STORAGE__URL` puts it there.

```yaml
services:
  orion:
    image: ghcr.io/goplasmatic/orion:1.1.0
    ports: ["8080:8080"]
    environment:
      ORION_STORAGE__URL: sqlite:/app/data/orion.db
      ORION_LOGGING__FORMAT: json
    volumes:
      - orion-data:/app/data
    stop_grace_period: 45s

volumes:
  orion-data:
```

**Pin the tag.** `latest` makes an upgrade something that happens to you rather
than something you decide.

## Configure it

Every setting is an `ORION_SECTION__KEY` environment variable, which is what
makes the image configurable without a config file:

```yaml
environment:
  ORION_STORAGE__URL: "postgres://user:pass@db:5432/orion"
  ORION_ADMIN_AUTH__ENABLED: "true"
  ORION_ADMIN_AUTH__API_KEYS: "${ORION_ADMIN_API_KEYS:?set this}"
  ORION_METRICS__ENABLED: "true"
  ORION_ENVIRONMENT: "production"
```

A name that is not a real setting is refused at startup with the nearest match,
rather than silently ignored, so a typo costs you a boot, not a week. Mount a
TOML file and pass `-c` if you prefer files; the environment still overrides it.

## Give the container time to drain

Orion's shutdown sequence takes `shutdown_drain_secs + shutdown_force_timeout_secs`
to complete. If Docker's grace period is shorter, the process is killed
mid-drain and in-flight requests die with it:

```yaml
stop_grace_period: 45s      # > ORION_SERVER__SHUTDOWN_DRAIN_SECS + FORCE_TIMEOUT_SECS
```

The sequence itself is described in
[Timeouts, Retries & Circuit Breakers](./failure-handling.md#shut-down-without-dropping-requests).

## Probe it

The image needs no extra configuration for health checks:

```yaml
healthcheck:
  test: ["CMD", "curl", "-f", "http://localhost:8080/healthz"]
  interval: 5s
  timeout: 3s
  start_period: 10s
  retries: 3
```

Use `/healthz` for "is the process alive" and `/readyz` for "should it get
traffic". A load balancer wants the second one.

## The reference HA topology

`docker-compose.ha.yml` in the repository root is the production shape as a
compose file: **nginx → 2× Orion in cluster mode → shared PostgreSQL + Redis**,
plus a one-shot `migrate` service that completes before either node boots.

```bash
export ORION_ADMIN_API_KEYS="$(openssl rand -hex 32)"
docker compose -f docker-compose.ha.yml up -d --wait
curl -s http://localhost:8080/health
```

It boots as `ORION_ENVIRONMENT=production`, so admin auth is enforced and
`ORION_ADMIN_API_KEYS` is required — the stack refuses to start without it,
which is the point. Set `ORION_CORS_ALLOWED_ORIGINS` if a browser dashboard
needs it.

What it demonstrates, and what to copy into your own topology:

- **`auto_migrate = false` on the replicas**, with migrations applied once by a
  separate service before either node starts.
- **Cluster mode on**, so config changes made through either node reach both,
  and dedup, response caches, and rate limits are shared.
- **`stop_grace_period` above the drain budget**, so `SIGTERM` runs the graceful
  sequence instead of being cut short.
- **A pinned image tag**, overridable with `ORION_VERSION`.

`deploy/ha/rolling-drill.sh` drives traffic through the load balancer while one
node is `SIGTERM`ed, and asserts every response was a 2xx — run it to prove the
drain settings on *your* hardware, not just on the reference one.

## Upgrade a compose deployment

1. **Back up the database**: a SQLite volume snapshot, or your database's own
   tooling. See [Back Up & Restore](./backup-restore.md).
2. **Read the version's upgrade guide** and run `orion-server preflight` with the
   new image against the old database. See [Upgrades](./upgrades.md).
3. **Bump the pinned tag** and, in a cluster, let the `migrate` service apply
   migrations before the replicas restart.
4. **Restart one node at a time** so the load balancer always has a node to send
   to. With a single node, there is a gap, which is the argument for the second
   one.

## Running without a container

Orion is also a plain binary, and nothing about it requires Docker: install it
from a release or Homebrew, put the config somewhere readable, and run
`orion-server -c /etc/orion/config.toml` under whatever supervises processes on
that host.

We ship container images and a Helm chart, not a systemd unit, so if you run it
under systemd, you write the unit file. Give it the same two properties this
page has been about: a durable path for the database, and a stop timeout longer
than the drain budget.

## Related

- [Cluster Mode & High Availability](./cluster.md): what the HA compose file is
  configuring, and why.
- [Deploy on Kubernetes (Helm)](./kubernetes.md): the same shape, on
  Kubernetes.
- [Production Checklist](./production-checklist.md): before this becomes
  production.
- [Configuration Reference](../reference/configuration.md): every environment
  variable named here.
