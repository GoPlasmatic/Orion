<!-- description: Deploy Orion on Kubernetes with the official Helm chart — cluster topology, shared PostgreSQL and Redis, health probes and rolling upgrades in one command. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Deploy on Kubernetes

Orion ships an official Helm chart that deploys the [cluster topology](./cluster.md) in one command. You get N stateless replicas in cluster mode behind a Service, with one shared PostgreSQL or MySQL and one shared Redis. A pre-upgrade migration Job runs first, and rolling deploys surge rather than dip.

Every release publishes the chart to GHCR as an OCI artifact, so there is no chart repository to add.

## Before you start

You need a Kubernetes cluster, Helm 3, `kubectl`, and access to GHCR. A production install also needs reachable PostgreSQL or MySQL and Redis services, an admin API key, and permission to create workloads, Services, Jobs and Secrets. The commands below pin chart `1.0.0`, matching the chart version in this checkout. Choose the chart version that matches the Orion release you intend to deploy.

The chart installs with `ORION_ENVIRONMENT=production` by default, which enforces admin auth and refuses permissive CORS at boot. A bare `helm install` does not come up until you provide admin API keys, or opt into the dev stack. The failure is loud at install time rather than silent in production.

## Try it with the dev stack

For a throwaway install, `devStack.enabled=true` runs a single-replica in-namespace PostgreSQL and Redis with no persistence guarantees, and wires Orion to them. Dev-stack installs run as `development`, so no admin keys are required:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion \
  --version 1.0.0 --set devStack.enabled=true
kubectl port-forward svc/orion 8080:8080
open http://localhost:8080/docs
```

Never use the dev stack in production. The bundled PostgreSQL and Redis are disposable.

## Install for production

A production install requires three inputs: a database URL, a Redis URL, and at least one admin API key. Inline is quickest:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion --version 1.0.0 \
  --set storage.url="postgres://orion:secret@my-postgres:5432/orion" \
  --set cluster.redisUrl="redis://my-redis:6379" \
  --set adminAuth.apiKeys="{$(openssl rand -hex 32)}"
```

Pre-existing Secrets keep credentials out of Helm values and release history. The chart reads the `storage-url` key from `storage.existingSecret` and the `api-keys` key, a comma-separated list, from `adminAuth.existingSecret`:

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

# Only needed for browser clients of the admin API, such as the Orion Console.
# Empty means no cross-origin access; "*" is refused in production at boot.
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

Install with it:

```bash
helm install orion oci://ghcr.io/goplasmatic/charts/orion \
  --version 1.0.0 -f values.yaml
```

Admin requests then need the key:

```bash
curl -H "Authorization: Bearer <key>" https://orion.example.com/api/v1/admin/engine/status
```

TLS terminates at the Ingress in this setup. The Ingress routes only the main HTTP port; the metrics listener is deliberately not exposed.

## Notable values

The important subset. The chart's [`values.yaml`](https://github.com/GoPlasmatic/Orion/blob/main/deploy/helm/orion/values.yaml) is the full annotated list:

<div class="table-filter" data-label="Filter values"></div>

| Value | Default | Meaning |
|---|---|---|
| `replicaCount` | `2` | Replicas (ignored when `autoscaling.enabled`) |
| `image.repository` / `image.tag` | `ghcr.io/goplasmatic/orion` / chart `appVersion` | Server image; empty tag tracks the chart's app version |
| `env` | `production` | Orion environment; any `prod*` value enforces admin auth and refuses a CORS wildcard |
| `storage.url` / `storage.existingSecret` | — | Database URL (required unless devStack); the Secret's `storage-url` key wins over the inline URL |
| `storage.autoMigrate` | `false` | Replicas never migrate at boot; refused as `true` on a production cluster install |
| `cluster.enabled` | `true` | Multi-instance coordination (dedup, response cache, rate limits through Redis) |
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
| `extraEnv` | `[]` | Additional `ORION_*` overrides (see [Server configuration](../../reference/configuration/index.md)) |
| `devStack.enabled` | `false` | Throwaway in-namespace PostgreSQL + Redis; dev/demo only |
| `tests.enabled` | `true` | Render the `helm test` hooks |

Misspelled values fail the render. The chart enforces `values.schema.json` on every `install`, `upgrade` and `template`, with unknown keys rejected. Every required value on this chart is a string, so without the schema a typo like `--set cluster.enabld=true` would silently no-op; instead it fails at once.

The pods run under a restricted security posture by default: non-root (UID 10001), read-only root filesystem, all capabilities dropped, RuntimeDefault seccomp. `/tmp` is an emptyDir, and `persistence.mountPath` (default `/app/data`) is the only durable writable path.

The metrics listener is dedicated and unauthenticated by design. On the main listener `/metrics` sits behind admin auth, and a scraper should not hold a credential that can also rewrite workflows. Keep it cluster-internal, or turn it off with `metrics.enabled=false`; the alerts in [What to alert on](../run/monitoring.md#what-to-alert-on) then have no scrape target.

### Plugins and schedules

Neither has a chart value. Both are `ORION_*` overrides through `extraEnv`, which every replica shares, and each has one consequence worth planning for:

```yaml
extraEnv:
  - name: ORION_PLUGINS__ENABLED
    value: "true"
  - name: ORION_CRON__ENABLED
    value: "false"
```

[Plugins](../../concepts/plugins.md) are off by default. Turning them on makes the sandbox's pooling allocator reserve `max_live_instances × max_memory_bytes` of address space at startup, 16 GiB with the defaults. It is virtual, not resident, so it does not belong in `resources.requests`. It does matter wherever a container limits virtual memory.

The [cron scheduler](../../guides/patterns/scheduled-workflows.md) is on by default, and every replica must agree. A mixed setting quarantines an active cron channel on the replicas that have it off and runs it on the rest. That is visible as `components.cron: degraded` on `/health`, but it is not what anyone meant. Nothing else needs configuring for a multi-replica install. Occurrence identity, claim leases and singletons all live in the shared database, and there is no leader to elect.

## Upgrade

Upgrade the release, keeping the values you set:

```bash
helm upgrade orion oci://ghcr.io/goplasmatic/charts/orion \
  --version <new-version> --reuse-values
```

- **Migrations run as a `pre-install` and `pre-upgrade` Job** (`<release>-migrate`), before any new pod starts. Replicas boot with `storage.auto_migrate=false` and refuse to start on a pending migration, so a failed migration stops the rollout rather than booting mismatched replicas.
- **Schema changes follow the expand and contract convention** across one release. The old replicas keep serving against the migrated schema while the new ones roll in, which is what makes a rolling upgrade with `maxUnavailable: 0, maxSurge: 1` safe.
- **Shutdown is graceful by construction.** On SIGTERM a replica withdraws readiness, keeps serving for `server.shutdownDrainSecs`, then waits up to `server.shutdownForceTimeoutSecs` for in-flight requests. `terminationGracePeriodSeconds` is derived as drain plus force timeout plus 10, so the kubelet never cuts the sequence short.

## Verify

The chart ships `helm test` hooks, inert until run:

```bash
helm test orion
```

`test-connectivity` runs the binary's own `test-connectivity` subcommand: it opens the storage pool, counts pending migrations, and probes Kafka when enabled. `test-api` checks `/health` and `/readyz`, and that the metrics port serves Prometheus exposition text without a credential. To inspect by hand:

```bash
kubectl get pods -l app.kubernetes.io/name=orion,app.kubernetes.io/instance=orion
kubectl port-forward svc/orion 8080:8080
curl -s http://localhost:8080/readyz     # 200 once the engine is built
curl -s http://localhost:8080/health     # component detail (database, engine)
```

## Troubleshooting

### A pod is not Ready and has not restarted

Boot is still in progress. The startup probe budgets up to 5 minutes before liveness kicks in. That covers the pending-migration check, the cluster Redis connect, connector loading and the engine build. `kubectl logs` shows which stage it is in.

### A pod crash-loops right after install

Most often a missing required input. With `env=production`, the default, Orion refuses to boot without admin keys, with a CORS wildcard, or with `storage.autoMigrate=true` on a cluster install. The log line names the offending setting.

### Replicas refuse to start after `helm upgrade`

A pending migration. Check the `<release>-migrate` Job's logs with `kubectl logs job/orion-migrate`.

### Nothing is scraping metrics

The install notes say so explicitly. Enable `metrics.serviceMonitor` (Operator), `metrics.podMonitor`, or `metrics.prometheusAnnotations`.

## Single-node SQLite

For a small single-node install the chart can run Orion against embedded SQLite on a PVC instead of an external database:

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

The combination matters. A ReadWriteOnce claim cannot serve a surge replica, hence `Recreate` and one replica. A hook Job cannot share the replica's volume, hence boot-time migration instead of the Job. Backups then land under `/app/data/backups`; see [Back up and restore](../maintain/backup-restore.md).

## Next steps

- [Run a cluster](./cluster.md): what this chart is configuring, and why each piece is there.
- [Deploy with Docker](./docker.md): the same shape without Kubernetes.
- [Production checklist](../production-checklist.md): before this serves real traffic.
- [Server configuration](../../reference/configuration/index.md): every `ORION_*` key `extraEnv` can set.
- [Chart source](https://github.com/GoPlasmatic/Orion/tree/main/deploy/helm/orion): templates, `values.yaml`, `values.schema.json`.
