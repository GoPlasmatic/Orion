# Orion Helm Chart

Deploys Orion in cluster mode: N replicas behind a Service, sharing one
Postgres/MySQL and one Redis, with a pre-upgrade migration Job, graceful
rolling deploys (`/readyz` drain), an optional HPA, and a PDB.

## Production install

Point the chart at your managed database and Redis, and provide at least
one admin API key — the chart installs with `ORION_ENVIRONMENT=production`, which
enforces admin auth and refuses permissive CORS at boot:

```bash
helm install orion ./deploy/helm/orion \
  --set storage.url="postgres://orion:secret@my-postgres:5432/orion" \
  --set cluster.redisUrl="redis://my-redis:6379" \
  --set adminAuth.apiKeys="{$(openssl rand -hex 32)}"
```

Or keep both the database URL and the admin keys out of values with
pre-existing Secrets (keys `storage-url` and `api-keys` respectively;
`api-keys` is a comma-separated list):

```bash
kubectl create secret generic orion-storage \
  --from-literal=storage-url="postgres://orion:secret@my-postgres:5432/orion"
kubectl create secret generic orion-admin-auth \
  --from-literal=api-keys="$(openssl rand -hex 32)"
helm install orion ./deploy/helm/orion \
  --set storage.existingSecret=orion-storage \
  --set adminAuth.existingSecret=orion-admin-auth \
  --set cluster.redisUrl="redis://my-redis:6379"
```

Admin requests then need the key: `curl -H "Authorization: Bearer <key>" …`.
Browser dashboards (e.g. Orion-ui) additionally need their origin in
`cors.allowedOrigins` — cross-origin access is denied by default.

Migrations run as a `pre-install`/`pre-upgrade` Job; replicas boot with
`storage.auto_migrate=false` and refuse to start on a pending migration.
Keep migrations expand/contract compatible across one release (see
CONTRIBUTING.md).

Setting `storage.autoMigrate=true` on a production cluster install is refused
at startup — replicas would race each other at boot. The dev/demo install
below is exempt: it runs as `development` and has no migrate Job, because its
database is created by the same release.

Every pod spec sets `enableServiceLinks: false`. Nothing here reads the
kubelet's Docker-style service variables, and with a Service named `orion` they
would otherwise fill each container's environment with `ORION_`-prefixed names
that are not Orion settings. This is hygiene, not a requirement: the server
only treats a name as a setting when it carries the `__` section separator, and
no service link does, so a hand-written manifest without the flag boots fine.

## Dev / demo install

Runs a throwaway in-namespace Postgres + Redis (no persistence guarantees):

```bash
helm install orion ./deploy/helm/orion --set devStack.enabled=true
kubectl port-forward svc/orion 8080:8080
open http://localhost:8080/docs
```

## Notable values

| Value | Default | Meaning |
|---|---|---|
| `replicaCount` | `2` | Replicas (ignored when `autoscaling.enabled`) |
| `storage.url` / `storage.existingSecret` | — | Database URL (required unless devStack) |
| `cluster.redisUrl` | — | Shared Redis (required unless devStack) |
| `env` | `production` | Orion environment; production enforces admin auth |
| `adminAuth.apiKeys` / `adminAuth.existingSecret` | — | Admin API keys (required unless devStack) |
| `cors.allowedOrigins` | `[]` | Browser origins for the admin API (empty = deny) |
| `cors.additionalAllowedHeaders` | `[]` | Extra request headers to admit, on top of the built-in set |
| `cors.additionalExposedHeaders` | `[]` | Extra response headers a page script may read |
| `cors.allowCredentials` | `false` | Allow cookie-based cross-origin requests; needs explicit origins |
| `cors.maxAgeSecs` | `""` | `Access-Control-Max-Age`; empty omits the header. Max `86400` |
| `server.shutdownDrainSecs` | `15` | Keep serving after readiness is withdrawn |
| `server.shutdownForceTimeoutSecs` | `20` | Bound on the post-drain in-flight wait |
| `autoscaling.enabled` | `false` | CPU-based HPA |
| `podDisruptionBudget.enabled` | `true` | `maxUnavailable: 1` |
| `strategy` | `maxUnavailable: 0, maxSurge: 1` | Rolling deploys never drop below `replicaCount` Ready replicas |
| `podSecurityContext` / `securityContext` | restricted | Non-root, read-only rootfs, no capabilities, RuntimeDefault seccomp |
| `startupProbe` | 5 min budget | Holds liveness off while boot migrates/connects/builds the engine |
| `affinity` | soft anti-affinity | Spreads server replicas across nodes; set to override |
| `topologySpreadConstraints` | `[]` | Rendered verbatim when set |
| `persistence.enabled` | `false` | PVC at `/app/data` for single-node SQLite installs |
| `extraEnv` | `[]` | Additional `ORION_*` overrides |
| `metrics.enabled` | `true` | Prometheus metrics on a dedicated listener |
| `metrics.port` | `9090` | Metrics container/Service port; must differ from `server.port` |
| `metrics.serviceMonitor.enabled` | `false` | Prometheus Operator `ServiceMonitor` (needs the CRD) |
| `metrics.podMonitor.enabled` | `false` | `PodMonitor` alternative; works without the Service port |
| `metrics.prometheusAnnotations` | `false` | `prometheus.io/*` pod annotations for annotation-based discovery |
| `tests.enabled` | `true` | Render the `helm test` hooks |

`terminationGracePeriodSeconds` is derived as drain + force timeout + 10 so
SIGTERM always completes the graceful sequence.

## Plugins and schedules

Neither has a value of its own — both are ordinary `ORION_*` overrides through
`extraEnv` — but each has a consequence a chart install has to plan for.

```yaml
extraEnv:
  - name: ORION_PLUGINS__ENABLED
    value: "true"
  - name: ORION_CRON__ENABLED
    value: "false"
```

**Plugins are off by default, and turning them on costs virtual address
space.** The sandbox's pooling allocator reserves
`max_live_instances × max_memory_bytes` at startup — 16 GiB with the defaults.
That is virtual, not resident, so it does not belong in `resources.requests`;
it does matter wherever a container limits *virtual* memory. Signing keys go
in `[plugins.trust]`; see the
[production checklist](https://docs.goplasmatic.io/operate/production-checklist.html).

**The cron scheduler is on by default, and every replica must agree.** With
`replicaCount > 1`, a mixed setting quarantines an active cron channel on the
replicas that have it off and runs it on the rest — visible on `/health` as
`components.cron: degraded`, but not what anyone meant. Set it in `extraEnv`,
which every replica shares, rather than per-pod. No coordination value is
needed beyond that: occurrence identity, claim leases and singletons all live
in the shared database.

## Metrics

Metrics are served by a **dedicated listener** on `metrics.port`, not on the
main HTTP port. On the main listener `/metrics` sits behind admin auth, so a
scraper would need an admin API key — a credential that can also rewrite
workflows and read trace payloads. Setting a dedicated address makes the main
listener stop serving `/metrics` entirely, and the scraper needs no
credential.

That listener is unauthenticated and plain HTTP (`server.tls` covers the main
listener only), so it is deliberately **not** routed by the Ingress, which
backends the named `http` port. Keep it reachable only from inside the
cluster, or turn it off with `metrics.enabled=false`.

With the Prometheus Operator installed:

```bash
helm upgrade orion ./deploy/helm/orion --reuse-values \
  --set metrics.serviceMonitor.enabled=true \
  --set metrics.serviceMonitor.labels.release=kube-prometheus-stack
```

The `labels` must match your Prometheus `serviceMonitorSelector`. Without the
Operator, use `metrics.prometheusAnnotations=true` for annotation-based
discovery. To keep the metrics port off the Service entirely, combine
`metrics.service.enabled=false` with `metrics.podMonitor.enabled=true`.

## Testing an install

```bash
helm test orion
```

Runs two hooks: `test-connectivity` (the binary's own subcommand — opens the
storage pool, counts pending migrations, probes Kafka when enabled) and
`test-api` (checks `/health` and `/readyz`, and that the metrics port serves
Prometheus exposition text without a credential).

## Values schema

`values.schema.json` is enforced on every `install`/`upgrade`/`template`.
Every required value on this chart is a string, so without it a misspelled key
(`--set cluster.enabld=true`) would silently no-op; it now fails the render.

The root filesystem is read-only: `/tmp` is an emptyDir and
`persistence.mountPath` (default `/app/data`) is the only durable writable
path. A single-node SQLite install
(`storage.url=sqlite:/app/data/orion.db`) wants `persistence.enabled=true`
with `cluster.enabled=false`, `replicaCount=1`, `strategy.type=Recreate`,
and `migrateJob.enabled=false` + `storage.autoMigrate=true`; backups then
land under `/app/data/backups`.
