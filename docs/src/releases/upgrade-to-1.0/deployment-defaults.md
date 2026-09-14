<!-- description: The Helm chart and docker-compose.ha.yml require admin API keys in Orion 1.0, and the chart's pod defaults are hardened with pinned images. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Deployment defaults

Break 5 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**What changed.** Both shipped deployment paths used to bring up an
**unauthenticated admin API**. They now default to `ORION_ENVIRONMENT=production` and
require admin API keys.

**How you'll notice.**

- **Helm:** `helm install` / `helm upgrade` fails at *template* time, before
  anything reaches the cluster:

  ```
  adminAuth.existingSecret or adminAuth.apiKeys is required: the chart defaults
  to a production install with admin auth enforced. Set devStack.enabled=true
  for a throwaway dev install.
  ```

- **HA compose:** `docker compose up` aborts with
  `set ORION_ADMIN_API_KEYS (comma-separated admin API keys)`.

**What to do.** For Helm, supply keys as a chart-managed Secret:

```bash
helm upgrade --install orion deploy/helm/orion \
  --set-string adminAuth.apiKeys[0]="$ORION_ADMIN_KEY"
```

or point at a Secret you manage, which must expose the key `api-keys`:

```yaml
adminAuth:
  existingSecret: orion-admin-keys
```

Escape hatch: `devStack.enabled=true` skips the check and forces `ORION_ENVIRONMENT=development`. A keyless non-dev install needs **both** `adminAuth.enabled=false` **and** `env=development` — with `env: production` and no keys the pod passes templating and then CrashLoops at config validation.

For HA compose, set the host-side variable (note the name differs from the container-side `ORION_ADMIN_AUTH__API_KEYS`):

```bash
export ORION_ADMIN_API_KEYS="key-one,key-two"
docker compose -f docker-compose.ha.yml up -d
```

**`ORION_ENVIRONMENT=production` forces three things**, and the second one
surprises people:

1. **Admin auth must be enabled and have at least one key**, or the server
   refuses to boot.
2. **CORS wildcard `*` is rejected.** The *default* `[cors] allowed_origins` is
   `["*"]`, so a config that never mentioned CORS at all, and booted fine
   before, now fails with
   `CORS wildcard '*' is not allowed when environment starts with 'prod'. Set explicit origins in [cors] allowed_origins`.
   Set explicit origins before you flip to production:

   ```toml
   [cors]
   allowed_origins = ["https://app.example.com"]
   ```

3. **`/docs` and `/api/v1/openapi.json` are not served** unless you opt back in
   with `server.docs.enabled = true`. See
   [`/docs` and the OpenAPI spec](./security-and-access.md#docs-and-the-openapi-spec-are-off-in-production).

Nothing else keys off `production` — logging and TLS are unaffected.

### The chart's pod defaults are hardened, and the images are pinned

**What changed.** Four defaults were wrong. The chart shipped with no `securityContext` at all, so it failed Pod Security Standards `restricted` and every policy scanner out of the box. It inherited Kubernetes' default `maxUnavailable: 25%`, which at two replicas removes a pod before its replacement is Ready, defeating the graceful-drain design. The migrate Job inlined the full `postgres://user:pass@…` URL into its pod spec when `storage.existingSecret` was unset. And the compose files floated on `:latest`.

Chart installs now run non-root with a **read-only root filesystem**: the only writable paths are an emptyDir at `/tmp` and the data volume at `/app/data`. No capabilities, `allowPrivilegeEscalation: false`, and the `RuntimeDefault` seccomp profile. Rolling deploys surge instead of dipping (`maxUnavailable: 0`, `maxSurge: 1`), a soft pod anti-affinity spreads replicas across nodes. A `startupProbe` on `/healthz` gives boot a five-minute budget before liveness (10 s period, 3 failures) takes over. The migrate Job reads the storage URL through `secretKeyRef` in every case.

**How you'll notice.**

- A workload that wrote anywhere else in the container filesystem now fails —
  override `podSecurityContext` / `securityContext` in values if you need it.
  `POST /api/v1/admin/backups` is one such writer: `storage.backup_dir`
  defaults to `./backups`, which is on the now read-only rootfs. Set
  `persistence.enabled=true` (the chart then points `backup_dir` at the data
  volume) or give `storage.backup_dir` a path under a writable mount.
- The cluster needs headroom for one extra replica during an upgrade, or
  override `strategy`. Setting `affinity` replaces the default anti-affinity
  verbatim.
- **Images built from this Dockerfile run as UID:GID `10001:10001`** instead of
  the previously auto-assigned system UID. Bind mounts and named volumes
  created by an older image (`/app/data` under Docker or compose) may need
  `chown -R 10001:10001`; on Kubernetes the chart's new `fsGroup: 10001`
  handles PVC ownership on mount. If you override `podSecurityContext`, carry
  the numeric `runAsUser`/`runAsGroup`/`fsGroup` forward or `runAsNonRoot`
  verification fails against the image's user.
- The migrate Job's hook-scoped Secret copy — `<release>-orion-storage-migrate`,
  rendered only when `storage.existingSecret` is unset — is not release-managed,
  so `helm uninstall` leaves it behind; delete it manually if you want it gone.
  The Secret the server replicas read is a normal release resource and is
  removed as usual.
- `docker-compose.yml`, `docker-compose.ha.yml` and
  `examples/packages/postgres-orders/docker-compose.yml` now pin
  `ghcr.io/goplasmatic/orion:${ORION_VERSION:-1.0.0}` instead of `:latest`. Set
  `ORION_VERSION` to move. Local HA builds moved to an override file that
  retags them `orion:local`, so `docker compose build` can no longer clobber
  the published tag:

  ```bash
  docker compose -f docker-compose.ha.yml -f docker-compose.ha.build.yml up -d --wait
  ```

**Single-node SQLite installs are now first-class.** Set `persistence.enabled=true`, which is a PVC at `/app/data` kept on uninstall. Set it together with `cluster.enabled=false`, `replicaCount=1`, `strategy.type=Recreate`, `migrateJob.enabled=false` and `storage.autoMigrate=true`. The `Recreate` strategy is needed because a ReadWriteOnce claim cannot serve a surge replica. Backups then land under `/app/data/backups`.

**Local `docker build` and `git_hash`.** The `.dockerignore` file excludes `.git/`. A locally built image therefore reports `git_hash=unknown` from `/health`, `/metrics` and `--version` unless you pass the SHA, as the published images now do:

```bash
docker build --build-arg GIT_HASH=$(git rev-parse --short HEAD) -t orion .
```

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
