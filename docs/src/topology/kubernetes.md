# Kubernetes (Helm)

Orion ships an official Helm chart that deploys the production topology from
[Dev & Prod Environments](./environments.md) in one command: **N stateless
replicas in cluster mode** behind a Service, sharing one **PostgreSQL/MySQL**
and one **Redis**, with a **pre-upgrade migration Job**, graceful rolling
deploys (`/readyz` drain), an optional HPA, and a PodDisruptionBudget.

The chart is linted and rendered in CI, and every release publishes it to
GHCR as an OCI artifact — no chart repository to add:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion --version 1.0.0
```

Or straight from a checkout of the repo:

```bash
helm install orion deploy/helm/orion
```

The chart installs with `ORION_ENVIRONMENT=production` by default, which
enforces admin auth and refuses permissive CORS at boot — so a bare
`helm install` **will not come up until you provide admin API keys** (or opt
into the dev stack below). That's deliberate: the failure is loud at install
time, not silent in production.

## Quick Start (dev/demo)

For a throwaway install, `devStack.enabled=true` runs a single-replica
in-namespace Postgres + Redis (no persistence guarantees) and wires Orion to
them automatically. devStack installs run as `development`, so no admin keys
are required:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion \
  --version 1.0.0 --set devStack.enabled=true
kubectl port-forward svc/orion 8080:8080
open http://localhost:8080/docs
```

Never use devStack in production — the bundled Postgres and Redis are
disposable dev services.

## Production Install

A production install requires three inputs: a database URL, a Redis URL, and
at least one admin API key.

**Inline** (quickest):

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion --version 1.0.0 \
  --set storage.url="postgres://orion:secret@my-postgres:5432/orion" \
  --set cluster.redisUrl="redis://my-redis:6379" \
  --set adminAuth.apiKeys="{$(openssl rand -hex 32)}"
```

**With pre-existing Secrets** (keeps credentials out of Helm values and
release history). The chart reads the `storage-url` key from
`storage.existingSecret` and the `api-keys` key (a comma-separated list) from
`adminAuth.existingSecret`:

```bash
kubectl create secret generic orion-storage \
  --from-literal=storage-url="postgres://orion:secret@my-postgres:5432/orion"
kubectl create secret generic orion-admin-auth \
  --from-literal=api-keys="$(openssl rand -hex 32)"
```

Then a minimal `values.yaml`:

```yaml
storage:
  existingSecret: orion-storage      # Secret key: storage-url
cluster:
  redisUrl: redis://my-redis:6379    # shared dedup / response cache / rate limits
adminAuth:
  existingSecret: orion-admin-auth   # Secret key: api-keys (comma-separated)

# Only needed for browser clients of the admin API (e.g. the Orion console).
# Empty = no cross-origin access; "*" is refused in production at boot.
cors:
  allowedOrigins:
    - https://console.example.com

ingress:
  enabled: true
  className: nginx
  hosts:
    - host: orion.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: orion-tls
      hosts:
        - orion.example.com
```

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion \
  --version 1.0.0 -f values.yaml
```

Admin requests then need the key:

```bash
curl -H "Authorization: Bearer <key>" https://orion.example.com/api/v1/admin/engine/status
```

TLS terminates at the Ingress in this setup. The Ingress routes only the main
HTTP port — the metrics listener (below) is intentionally not exposed.

## Notable Values

The important subset — see the chart's
[`values.yaml`](https://github.com/GoPlasmatic/Orion/blob/main/deploy/helm/orion/values.yaml)
for the full annotated list:

| Value | Default | Meaning |
|---|---|---|
| `replicaCount` | `2` | Replicas (ignored when `autoscaling.enabled`) |
| `image.repository` / `image.tag` | `ghcr.io/goplasmatic/orion` / chart `appVersion` | Server image; empty tag tracks the chart's app version |
| `env` | `production` | Orion environment; any `prod*` value enforces admin auth and refuses a CORS wildcard |
| `storage.url` / `storage.existingSecret` | — | Database URL (required unless devStack); the Secret's `storage-url` key wins over the inline URL |
| `storage.autoMigrate` | `false` | Replicas never migrate at boot; refused as `true` on a production cluster install |
| `cluster.enabled` | `true` | Multi-instance coordination (dedup, response cache, rate limits via Redis) |
| `cluster.redisUrl` | — | Shared Redis (required when `cluster.enabled` unless devStack) |
| `adminAuth.apiKeys` / `adminAuth.existingSecret` | — | Admin API keys (required unless devStack); Secret key `api-keys`, comma-separated |
| `cors.allowedOrigins` | `[]` | Browser origins for the admin API (empty = deny) |
| `migrateJob.enabled` | `true` | Pre-install/pre-upgrade `orion-server migrate` Job (`backoffLimit: 3`) |
| `server.shutdownDrainSecs` | `15` | Keep serving after readiness is withdrawn on SIGTERM |
| `server.shutdownForceTimeoutSecs` | `20` | Bound on the post-drain in-flight wait |
| `metrics.enabled` | `true` | Prometheus metrics on a dedicated listener |
| `metrics.port` | `9090` | Metrics container/Service port (separate from `server.port` 8080) |
| `metrics.serviceMonitor.enabled` | `false` | Prometheus Operator `ServiceMonitor` (needs the CRD; set `labels` to match your `serviceMonitorSelector`) |
| `metrics.podMonitor.enabled` | `false` | `PodMonitor` alternative; works with `metrics.service.enabled=false` |
| `metrics.prometheusAnnotations` | `false` | `prometheus.io/*` pod annotations for annotation-based discovery |
| `ingress.enabled` | `false` | Ingress for the main HTTP port only (never the metrics port) |
| `resources` | `250m` CPU / `256Mi` req, `512Mi` limit | Container resources |
| `autoscaling.enabled` | `false` | CPU-based HPA (min `2`, max `6`, target `75%`) |
| `podDisruptionBudget.enabled` | `true` | `maxUnavailable: 1` during voluntary disruptions |
| `networkPolicy.enabled` | `false` | Ingress on the HTTP/metrics ports + egress rules you declare; with no egress rules the pod gets DNS and nothing else (fail-closed). The network-level pairing for `allow_private_urls` |
| `strategy` | `RollingUpdate`, `maxUnavailable: 0`, `maxSurge: 1` | Deploys never drop below `replicaCount` Ready replicas |
| `persistence.enabled` | `false` | PVC at `/app/data` for single-node SQLite installs |
| `extraEnv` | `[]` | Additional `ORION_*` overrides (see the [Config Reference](../configuration/reference.md)) |
| `devStack.enabled` | `false` | Throwaway in-namespace Postgres + Redis — dev/demo only |
| `tests.enabled` | `true` | Render the `helm test` hooks |

**Misspelled values fail the render.** The chart enforces
`values.schema.json` on every `install`/`upgrade`/`template`, with unknown
keys rejected. Every required value on this chart is a string, so without the
schema a typo like `--set cluster.enabld=true` would silently no-op; instead
it fails immediately.

The pods run under a restricted security posture by default: non-root
(UID 10001), read-only root filesystem, all capabilities dropped, RuntimeDefault
seccomp. `/tmp` is an emptyDir and `persistence.mountPath` (default
`/app/data`) is the only durable writable path.

The metrics listener is dedicated and unauthenticated by design — on the main
listener `/metrics` sits behind admin auth, and a scraper should not hold a
credential that can also rewrite workflows. Keep it cluster-internal, or turn
it off with `metrics.enabled=false` (the 1.0 operational alerts then have no
scrape target).

## Upgrades

```bash
helm upgrade orion oci://ghcr.io/goplasmatic/charts/orion \
  --version <new-version> --reuse-values
```

- **Migrations run as a `pre-install`/`pre-upgrade` Job**
  (`<release>-migrate`), before any new pod starts. Replicas boot with
  `storage.auto_migrate=false` and refuse to start on a pending migration, so
  a failed migration stops the rollout rather than booting mismatched
  replicas.
- **Schema changes follow the expand/contract convention** across one
  release: the old replicas keep serving against the migrated schema while
  the new ones roll in. That is what makes a rolling upgrade with
  `maxUnavailable: 0, maxSurge: 1` safe.
- **Shutdown is graceful by construction.** On SIGTERM a replica withdraws
  readiness, keeps serving for `server.shutdownDrainSecs`, then waits up to
  `server.shutdownForceTimeoutSecs` for in-flight requests;
  `terminationGracePeriodSeconds` is derived as drain + force timeout + 10 so
  the kubelet never cuts the sequence short.

## Verify & Troubleshoot

The chart ships `helm test` hooks (inert until run):

```bash
helm test orion
```

- `test-connectivity` — runs the binary's own `test-connectivity` subcommand:
  opens the storage pool, counts pending migrations, probes Kafka when
  enabled.
- `test-api` — checks `/health` and `/readyz`, and that the metrics port
  serves Prometheus exposition text without a credential.

To inspect by hand:

```bash
kubectl get pods -l app.kubernetes.io/name=orion,app.kubernetes.io/instance=orion
kubectl port-forward svc/orion 8080:8080
curl -s http://localhost:8080/readyz     # 200 once the engine is built
curl -s http://localhost:8080/health     # component detail (database, engine)
```

Common symptoms:

- **Pod not Ready, no restarts** — boot is still in progress. The startup
  probe budgets up to 5 minutes for the pending-migration check, cluster
  Redis connect, connector loading, and engine build before liveness kicks
  in. Check `kubectl logs` for which stage it's in.
- **CrashLoop right after install** — most often a missing required input.
  With `env=production` (the default) Orion refuses to boot without admin
  keys, with a CORS wildcard, or with `storage.autoMigrate=true` on a cluster
  install. The log line names the offending setting.
- **Replicas refusing to start after `helm upgrade`** — a pending migration:
  check the `<release>-migrate` Job's logs
  (`kubectl logs job/orion-migrate`).
- **Nothing scraping metrics** — the install notes say so explicitly; enable
  `metrics.serviceMonitor` (Operator), `metrics.podMonitor`, or
  `metrics.prometheusAnnotations`.

## Single-Node SQLite

For a small single-node install the chart can run Orion against embedded
SQLite on a PVC instead of an external database:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion --version 1.0.0 \
  --set storage.url="sqlite:/app/data/orion.db" \
  --set persistence.enabled=true \
  --set cluster.enabled=false \
  --set replicaCount=1 \
  --set strategy.type=Recreate \
  --set migrateJob.enabled=false \
  --set storage.autoMigrate=true \
  --set adminAuth.apiKeys="{$(openssl rand -hex 32)}"
```

The combination matters: a ReadWriteOnce claim cannot serve a surge replica
(hence `Recreate` and one replica), and a hook Job cannot share the replica's
volume (hence boot-time migration instead of the Job). Backups then land
under `/app/data/backups` — see
[Maintainability](../features/maintainability.md).

## See Also

- [Dev & Prod Environments](./environments.md) — the topology this chart deploys
- [Deployability](../features/deployability.md) — all distribution channels
- [Scalability](../features/scalability.md) and [Availability](../features/availability.md) — cluster-mode behavior
- [Config Reference](../configuration/reference.md) — every `ORION_*` key `extraEnv` can set
- [Chart source](https://github.com/GoPlasmatic/Orion/tree/main/deploy/helm/orion) — templates, `values.yaml`, `values.schema.json`
