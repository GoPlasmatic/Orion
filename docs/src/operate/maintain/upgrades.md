<!-- description: How to move an existing Orion deployment from one version to another: the per-version guides, the expand and contract migration rule, and the preflight scan. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-19 -->

# Upgrade an instance

An upgrade is a binary swap plus a database migration. Channels, workflows and connectors stay where they are; they live in the database, not in the binary, so nothing is redeployed and nothing is re-imported. This guide is the order that works, and how a stale name fails on each surface.

## Before you start

You need the new binary, admin access to the instance and its database, and the release's [upgrade guide](../../releases/index.md). Read that guide before step 1, not after step 5. Each one lists what changes, with a detection command for each item.

## The order that works

1. **Back up the database.** Every later step is reversible if this one happened. See [Back up and restore](./backup-restore.md).
2. **Run `orion-server preflight` with the new binary against the old database.** It is read-only, needs only `storage.url`, and names every stored channel and workflow the new version would refuse. This is the step that turns an upgrade's surprises into a list you can work through beforehand.
3. **Run `orion-server validate-config`** against the config you will deploy. Preflight reads the database; this reads the config file and the `ORION_*` environment, which fail differently (see [How a rename fails, by surface](#how-a-rename-fails-by-surface)).
4. **Migrate.** In a single-node deployment, migrations run at boot. In a cluster, run `orion-server migrate` as a deploy step and keep `storage.auto_migrate = false`. `migrate --wait 60s` waits for a database that is still starting, so the step needs no retry loop around it. A production cluster that tries to migrate at boot is refused at startup rather than allowed to race.
5. **Roll the fleet.** `/readyz` flips to `503` on `SIGTERM` while the node keeps serving through its drain window, so a rolling deploy sheds no requests.

## How a rename fails, by surface

Names change between versions. Orion never accepts a retired spelling silently. The two surfaces that carry names fail differently, because they are edited at different times by different people:

| Surface | Owner | How a stale name fails |
|---|---|---|
| Config file and `ORION_*` environment | Operator, edited at deploy time | **Startup error** naming the replacement. Unknown keys are refused, and every renamed environment variable is listed in a retired-names table so the message says what to set instead. |
| Channel config and workflow JSON | Author, stored in the database | **Refused at create and update**, and **quarantined at load** if already stored. The channel is refused at every ingress rather than served with a guard missing. |

Nobody hand-edits stored channel and workflow rows during an upgrade. That surface cannot rely on a startup error the way the config file does. Quarantine is the equivalent: loud, fail-closed, and visible on `/health` and the admin surface.

<details><summary>Why are old spellings refused rather than accepted?</summary>

The cost of a silently accepted old name decides this. `cors` → `origin_allow_list` is the clearest case. Had the old key parsed and been dropped, every channel using it would have served with no origin allow-list. That is indistinguishable from a channel that deliberately checks nothing. The failure would have been silent, permanent, and a security regression. The same argument applies to `backpressure.max_concurrent`, whose replacement means something different: per node, not per cluster. Accepting it under the new field would admit N× the intended concurrency.

</details>

`orion-server preflight` exists to move both failures earlier. It reads the stored estate and the environment and names every entity that fails. You see them before the rollout rather than during it.

## Breaking upgrade path: v1 to v2

Orion v2 replaces the `/api/v1/*` route prefixes with `/api/v2/*` and introduces immutable candidate deployment revisions, content-addressed connector provider packages, and versioned connections:

1. **HTTP route prefixes move from `/api/v1/*` to `/api/v2/*`:**
   - Data plane: `/api/v1/data/{*path}` → `/api/v2/data/{*path}` (including `/async` submissions).
   - Admin plane: `/api/v1/admin/*` → `/api/v2/admin/*`.
   - OpenAPI specification: `/api/v1/openapi.json` → `/api/v2/openapi.json`.
   - Requests sent to `/api/v1/*` are no longer mounted and return `404 Not Found`. Update API gateways, ingress rules, scrapers, and client base URLs before rolling the binary.
2. **Generate a v1 → v2 migration report and candidate bundle:**
   - Use `POST /api/v2/admin/migration-report` or `orion-cli deployments migration-report -f legacy-estate.json` to map legacy v1 connectors (`http`, `db`, `cache`, `kafka`, `smtp`, `storage`), workflows (`http_call`, `db_read`, `db_write`), and channels into canonical v2 provider packages, `connector_call` operations, connection revisions, and a candidate deployment manifest.
3. **Verify edition admission compatibility before cutover:**
   - Use `POST /api/v2/admin/edition-transition-preflight` or `orion-cli deployments edition-preflight` to confirm every referenced provider package digest satisfies the target edition's admission policy.
4. **Plan, apply, and verify convergence:**
   - Upload and admit provider packages (`orion-cli providers upload --admit`), create connection revisions (`orion-cli connections create`), then plan and apply the deployment manifest (`orion-cli deployments plan` and `orion-cli deployments apply --wait`).

## What a version number promises

Orion follows semantic versioning, and 1.0 froze the surfaces that matter to callers and operators. Those are the data-plane request and response shapes, the admin API paths and envelopes, the config keys, and the metric names. Breaking any of them requires a major version. [Versioning and support policy](../../releases/versioning-policy.md) states the policy, the supported-version window, the MSRV, and the platform matrix.

## Verify

After the roll, confirm every node serves the new version and holds the whole estate:

```bash
curl -s http://localhost:8080/health | jq '{version, status, workflows_loaded, quarantined: .channels.quarantined}'
```

`version` is the new number, `status` is `ok`, `workflows_loaded` matches what it was before the upgrade, and `quarantined` is empty. Anything preflight named should have been fixed before this step; anything still listed here is the place to start.

## Next steps

- [Releases](../../releases/index.md): the per-version upgrade guides, and what each release changed.
- [CLI](../../reference/cli/index.md): `preflight`, `validate-config`, `migrate` and `test-connectivity` in full.
- [Server configuration](../../reference/configuration/index.md): every key and its environment variable, the authority when a config error names one.
- [Back up and restore](./backup-restore.md): step one, in detail.
