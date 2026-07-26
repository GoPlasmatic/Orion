# Orion Helm Chart

Deploys Orion in cluster mode: N replicas behind a Service, sharing one
Postgres/MySQL and one Redis, with a pre-upgrade migration Job, graceful
rolling deploys (`/readyz` drain), an optional HPA, and a PDB.

## Production install

Point the chart at your managed database and Redis:

```bash
helm install orion ./deploy/helm/orion \
  --set storage.url="postgres://orion:secret@my-postgres:5432/orion" \
  --set cluster.redisUrl="redis://my-redis:6379"
```

Or keep the database URL out of values with a pre-existing Secret
(key `storage-url`):

```bash
kubectl create secret generic orion-storage \
  --from-literal=storage-url="postgres://orion:secret@my-postgres:5432/orion"
helm install orion ./deploy/helm/orion \
  --set storage.existingSecret=orion-storage \
  --set cluster.redisUrl="redis://my-redis:6379"
```

Migrations run as a `pre-install`/`pre-upgrade` Job; replicas boot with
`storage.auto_migrate=false` and refuse to start on a pending migration.
Keep migrations expand/contract compatible across one release (see
CONTRIBUTING.md).

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
| `server.shutdownDrainSecs` | `15` | Keep serving after readiness is withdrawn |
| `server.shutdownForceTimeoutSecs` | `20` | Bound on the post-drain in-flight wait |
| `autoscaling.enabled` | `false` | CPU-based HPA |
| `podDisruptionBudget.enabled` | `true` | `maxUnavailable: 1` |
| `extraEnv` | `[]` | Additional `ORION_*` overrides |

`terminationGracePeriodSeconds` is derived as drain + force timeout + 10 so
SIGTERM always completes the graceful sequence.
