<!-- description: Upgrade an Orion instance: a binary swap and migrations within 2.x, a fresh state database and a redeploy when moving from 1.x. -->
<!-- type: guide -->
<!-- last_verified: 2026-10-25 -->

# Upgrade an instance

Within Orion 2, an upgrade is a binary swap plus a database migration. Moving from Orion 1.x is different: there is no in-place upgrade, so you start a fresh state database and deploy your definitions onto it.

## Before you start

You need the new binary, admin access to the instance and its database, and the definition set you deploy, in a repository. Read the release's notes in [Releases](../../releases/index.md) before step 1. Each lists what changes and how to detect what it affects.

## The order that works

This order is for an upgrade within Orion 2. The committed deployment lives in the state database, not in the binary. A new binary builds the same head it found, so nothing is redeployed.

1. **Back up the database.** Every later step is reversible if this one happened. See [Back up and restore](./backup-restore.md).
2. **Run `orion-server lint` with the new binary** over your definition set. It names every channel and workflow the new version would refuse, before any node tries to build it.
3. **Run `orion-server validate-config`** against the config you will deploy. Lint reads the definitions. This reads the config file and the `ORION_*` environment, which fail differently (see [How a rename fails, by surface](#how-a-rename-fails-by-surface)).
4. **Migrate.** On a single node, migrations run at boot. In a cluster, run `orion-server migrate` as a deploy step and keep `storage.auto_migrate = false`. `migrate --wait 60s` waits for a database that is still starting.
5. **Roll the fleet.** `/readyz` answers `503` on `SIGTERM` while the node keeps serving through its drain window, so a rolling deploy sheds no requests.

If step 2 found a definition the new binary refuses, fix it first. Compile and apply the fix with the old binary still serving, then upgrade.

## Move from Orion 1.x

Orion 2 starts its schema over, with one baseline migration per backend. It cannot read a 1.x state database, and no tool converts one. A 1.x database is refused at startup with this message:

```text
Error: Configuration error: the state database at storage.url holds 1 migration(s) this binary does not ship (versions [2]). An Orion 1.x database cannot be upgraded in place: Orion 2 starts from a fresh state database. Point storage.url at a new database and deploy your definitions with `orion-server compile` and `orion-cli deployments apply -d`.
```

The count and the version list depend on how far the 1.x database was migrated. To move:

1. **Keep the 1.x database.** Leave the old instance serving until the new one is proven. Nothing in Orion 2 writes to it.
2. **Get the definitions as files.** Orion 2 reads workflows, channels and connectors only from a definition set. If yours exist only in the 1.x database, export them with the 1.x tooling while it still runs.
3. **Lint and compile them with the Orion 2 binary.** `orion-server lint ./definitions`, then `orion-server compile ./definitions -o dist/deployment`. Every `db` connector must name its [`driver`](../../reference/connectors/db.md#the-driver-field), and a literal credential is refused. Replace it with an `env://` or `vault://` reference.
4. **Point `storage.url` at a new, empty database.** Start Orion 2 against it. The baseline migration creates the schema.
5. **Deploy.** `orion-cli deployments apply -d dist/deployment` commits revision 1. The node builds it and serves it.
6. **Move the clients.** Every path moved from `/api/v1/` to `/api/v2/`, as the next section lists.

Traces, audit logs, the trace DLQ and the cron ledger stay in the 1.x database. Keep it, or its backup, for as long as you need those records.

### The route prefixes moved

Orion 2 serves every HTTP route under `/api/v2/`. Requests to `/api/v1/` answer `404`, so update gateways, ingress rules, scrapers and client base URLs before you cut traffic over.

| Orion 1.x | Orion 2 |
|---|---|
| `/api/v1/data/{*path}` and its `/async` form | `/api/v2/data/{*path}` |
| `/api/v1/admin/*` | `/api/v2/admin/*`. The per-entity routes are gone; see [Deployment endpoints](../../reference/admin-api/deployments.md) |
| `/api/v1/openapi.json` | `/api/v2/openapi.json` |

## How a rename fails, by surface

Names change between versions. Orion never accepts a retired spelling silently. The two surfaces that carry names fail differently, because different people edit them at different times:

| Surface | Owner | How a stale name fails |
|---|---|---|
| Config file and `ORION_*` environment | Operator, edited at deploy time | **Startup error** naming the replacement. Unknown keys are refused, and a renamed environment variable is named with what to set instead. |
| Channel config and workflow JSON | Author, deployed as definitions | **Refused** by `lint`, `compile` and the deployment plan. A head already committed that a new binary cannot load **quarantines** the channel, which is then refused at every ingress. |

Nobody edits deployed definitions during an upgrade, so that surface cannot rely on a startup error. Quarantine is the equivalent: loud, fail-closed, and visible on `/health`.

<details><summary>Why are old spellings refused rather than accepted?</summary>

The cost of a silently accepted old name decides this. `cors` → `origin_allow_list` is the clearest case. Had the old key parsed and been dropped, every channel using it would serve with no origin allow-list. That is indistinguishable from a channel that deliberately checks nothing. The same argument applies to `backpressure.max_concurrent`, whose replacement is per node rather than per cluster. Accepting it under the new field would admit N times the intended concurrency.

</details>

## Verify

After the roll or the move, confirm every node serves the new version and the whole deployment:

```bash
curl -s http://localhost:8080/health | jq '{version, status, workflows_loaded, quarantined: .channels.quarantined}'
curl -s http://localhost:8080/api/v2/admin/deployments | jq '{head_revision, applied_revision}'
```

`version` is the new number and `status` is `ok`. `workflows_loaded` matches the workflows your deployment carries, and `quarantined` is empty. `applied_revision` equals `head_revision`. Anything still listed under `quarantined` is the place to start.

## Next steps

- [Releases](../../releases/index.md): the per-version notes, and what each release changed.
- [Deployments](../../concepts/deployments.md): how a definition set becomes the head a node serves.
- [CLI](../../reference/cli/index.md): `validate-config`, `migrate`, `lint`, `compile` and `test-connectivity` in full.
- [Back up and restore](./backup-restore.md): step one, in detail.
