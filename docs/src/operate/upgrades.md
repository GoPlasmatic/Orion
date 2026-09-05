<!-- description: How to move an existing Orion deployment from one version to another: the per-version guides, the expand and contract migration rule, and the preflight scan. -->
# Upgrades

You do not need this page to run Orion. Read it when you are moving an existing
deployment from one Orion version to another.

An upgrade is a binary swap plus a database migration. Channels, workflows, and
connectors stay where they are — they live in the database, not in the binary,
so nothing is redeployed and nothing is re-imported.

## The order that works

1. **Back up the database.** Every later step is reversible if this one
   happened. See [Back Up & Restore](./backup-restore.md).
2. **Run `orion-server preflight` with the new binary against the old
   database.** It is read-only, needs only `storage.url`, and names every stored
   channel and workflow the new version would refuse. This is the step that
   turns an upgrade's surprises into a list you can work through beforehand.
3. **Run `orion-server validate-config`** against the config you will deploy.
   Preflight reads the database; this reads the config file and the `ORION_*`
   environment, which fail differently (see below).
4. **Migrate.** In a single-node deployment, migrations run at boot. In a
   cluster, run `orion-server migrate` as a deploy step and keep
   `storage.auto_migrate = false` — a production cluster that tries to migrate
   at boot is refused at startup rather than allowed to race.
5. **Roll the fleet.** `/readyz` flips to 503 on `SIGTERM` while the node keeps
   serving through its drain window, so a rolling deploy sheds no requests.

Read the version's upgrade guide before step 1, not after step 5. Each one lists
what changes with a detection command for each item.

## How a rename fails, by surface

Names change between versions. Orion never accepts a retired spelling silently,
but the two surfaces that carry names fail differently, because they are edited
at different times by different people:

| Surface | Owner | How a stale name fails |
|---|---|---|
| Config file and `ORION_*` environment | Operator, edited at deploy time | **Startup error** naming the replacement. Unknown keys are refused, and every renamed environment variable is listed in a retired-names table so the message says what to set instead. |
| Channel config and workflow JSON | Author, stored in the database | **Refused at create/update**, and **quarantined at load** if already stored — the channel is refused at every ingress rather than served with a guard missing. |

Nobody hand-edits stored channel and workflow rows during an upgrade, which is
why that surface cannot rely on a startup error the way the config file does.
Quarantine is the equivalent: loud, fail-closed, and visible on `/health` and
the admin surface.

<details><summary>Why old spellings are refused rather than accepted</summary>

The cost of a silently-accepted old name decides this. `cors` →
`origin_allow_list` is the clearest case: had the old key parsed and been
dropped, every channel using it would have served with no origin allow-list,
indistinguishable from a channel that deliberately checks nothing. The failure
would have been silent, permanent, and a security regression. The same argument
applies to `backpressure.max_concurrent`, whose replacement means something
different (per node, not per cluster) — accepting it under the new field would
admit N× the intended concurrency.

</details>

`orion-server preflight` exists to move both failures earlier. It reads the
stored estate and the environment and names every entity that will fail, so you
see them before the rollout rather than during it.

## What a version number promises

Orion follows semantic versioning, and 1.0 froze the surfaces that matter to
callers and operators: the data-plane request and response shapes, the admin API
paths and envelopes, the config keys, and the metric names. Breaking any of them
requires a major version.

[Support & Compatibility](../reference/support.md) states the policy, the
supported-version window, the MSRV, and the platform matrix.

## Per-version guides

- [Upgrading to 1.6.0](./upgrading-to-1.6.md): from 1.5.x. A minor release
  that adds the plugin sandbox, off by default: three expand-only migrations,
  a larger binary, and an MSRV rule that now follows Wasmtime's.
- [Upgrading to 1.2.0](./upgrading-to-1.2.md): from 1.1.x. A minor release:
  seven behaviour changes, two of them breaking for offline test suites and
  the CLI's removed MCP server.
- [Upgrading to 1.1.0](./upgrading-to-1.1.md): from 1.0.x. A minor release:
  five behaviour changes, no renames, no rewrites. One of them can start
  refusing traffic on a channel that appeared to work.
- [Upgrading to 1.0.0](./upgrading-to-1.0.md): from 0.3.0. Every break, grouped
  by type, with the preflight scanner that finds the database-backed ones.

## Related

- [CLI Reference](../reference/cli.md): `preflight`, `validate-config`,
  `migrate`, and `test-connectivity` in full.
- [Configuration Reference](../reference/configuration.md): every key and its
  environment variable, the authority when a config error names one.
- [CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md) —
  what was added, as opposed to what broke.
