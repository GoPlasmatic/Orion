<!-- description: Move an Orion service between instances as one versioned package — export the closure, lint and plan with zero writes, then apply and record a receipt. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-19 -->

# Promote between environments

Promotion moves one service from one instance to another: dev to QA to production, or a template instance out to every region. The unit is a [package](../../concepts/packages.md): a service's channels, their workflows, and the connectors, plugins and models those workflows reference, as one versioned artifact. Five verbs, one artifact file, and no shared storage between instances; the file is the only thing that travels.

## Before you start

You need `orion-server` on the machine that runs the verbs, and network access to each instance. Every instance with `admin_auth.enabled = true`, which production should be, needs an admin token:

```bash
export ORION_ADMIN_TOKEN=…    # sent as the admin bearer token
```

Every subcommand except `lint` calls an instance's admin API, and an unset token means every call is refused. `lint` needs no server and no token, which is what lets it run as a CI gate on a runner holding no production credentials.

## The five verbs

Export from the source, lint offline, plan against the target, apply, and diff:

```bash
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json

orion-server package lint  -f payments-1.4.0.json
orion-server package plan  -s https://qa.orion.internal   -f payments-1.4.0.json
orion-server package apply -s https://qa.orion.internal   -f payments-1.4.0.json
orion-server package diff  -s https://prod.orion.internal -f payments-1.4.0.json
```

| Verb | Needs | Writes | What it does |
|------|-------|--------|--------------|
| `export` | Source instance | Nothing | Capture selected channels plus their closure into one versioned artifact |
| `lint` | Nothing; fully offline | Nothing | Validate entity shapes with the same validators the POST endpoints run, check closure completeness against `requires`, verify the content hash |
| `plan` | Target instance | **Nothing** | Pre-flight: receipt immutability, the exact per-entity action `apply` would take, `requires` verification, every activation gate |
| `apply` | Target instance | Everything | Claim the receipt, stage all entities, activate in dependency order, reload once, flip the receipt to applied |
| `diff` | Any instance | Nothing | Compare the instance's content hashes against the artifact's; exits non-zero on drift |

Use `lint` as the PR gate and `plan` as the pre-deploy gate. Use `diff` as the post-deploy check and as a scheduled job, because it is how you learn that production drifted from what you shipped.

> [!NOTE]
> `export` is one way to obtain an artifact; [`orion-server compile`](../../reference/cli/orion-server/compile.md) is the other. It builds the same shape from a directory of definitions with no instance to export from, and resolves the set's shared `constants`, `errors` and `fragments` on the way, which is why it exists: the admin API has no set to resolve them against. `lint`, `plan`, `apply` and `diff` cannot tell the two apart.

> [!TIP]
> A deploy that runs on every image build can name the version after the content: `compile --version content` writes `content-<12 hex>` from the artifact's own hash. Unchanged definitions compile to the version already applied, so the apply is a no-op, and a revert compiles to the earlier version and rolls back to it. See [Content versions](../../reference/cli/orion-server/compile.md#content-versions).

## Select what ships

Membership is a tag. Give every entity of a service the same label when you create it:

```json
{ "channel_id": "payments", "tags": ["pkg:payments"], "...": "..." }
```

Export selects channels, by tag or by explicit id, and computes the closure from there. Each channel brings its workflow, and each workflow brings every connector it references:

```bash
# Everything tagged pkg:payments, plus the closure
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json

# …or hand-pick channels by id
orion-server package export -s https://dev.orion.internal \
  --channels payments,payment-refunds --name payments --version 1.4.0 \
  -o payments-1.4.0.json
```

A `channel_call` target outside the selection is not pulled in. It lands in the artifact's `requires` block, on the theory that a channel you did not select belongs to somebody else's package. `plan` then verifies each requirement exists and is active on the target before anything is written.

## What `apply` does, in order

Knowing the phases is what lets you interpret a failure:

1. **Claim the receipt as `staged`.** This is the atomic immutability check. A reused applied version with different content is refused here, and it doubles as the guard against two applies running at once.
2. **Stage every entity as a draft**, in dependency order: plugins, then connectors, then models, then workflows, then channels. A package that carries [plugins](../../concepts/plugins.md) activates them here, reload included, before anything else is staged. A workflow's create-time gate validates every function it names against the registry the engine is serving, so a workflow calling a plugin function cannot be staged until the plugin is active and loaded. Connector import reloads the connector registry server-side, so workflow activation later sees them.
3. **Activate in dependency order, with the reload deferred.** Each activation is marked in the database but the engine is not rebuilt yet.
4. **Reload the engine once**, which is also one config-epoch bump in a cluster.
5. **Flip the receipt to `applied`.** The receipt records the version's inventory: the ids of every entity it carried.

Two properties fall out of that ordering. However many entities the package carries, the running engine rebuilds once. A package that brings plugins rebuilds it twice: once to admit them and once for everything else. Every replica converges on the whole package, never on a half-applied one. And every call is stamped with `X-Orion-Change-Context: package=<name>@<version>`, so the [audit trail](../run/audit-logs.md) filters back into the promotion that caused it.

A plugin travels with its component only when the export was run with `--include-artifacts`. Otherwise it travels as manifest and digest, and `plan` refuses a target that does not already hold that digest, naming the flag. A plugin the source itself no longer serves at the digest the workflows ran against is recorded under `requires.plugins`. `plan` checks the target has it active, the same boundary a `channel_call` outside the selection makes.

A model never travels as bytes. It is carried as its manifest and its artifact reference: the storage connector, the key and the digest. The target fetches the object through *its own* connector of that name when it admits the model. Every such connector the package does not carry is recorded under `requires.storage` and checked by `plan` and `apply` before anything is written. A model a workflow names that the source does not serve active is recorded under `requires.models`, which `plan` checks the target serves. Activation is refused until a node has admitted the artifact, so `apply` waits for the target's verdict on each model it staged. It polls `GET /models/{id}` with progress on stderr and a 900 s ceiling, then activates the model ahead of the workflows that name it.

## When an apply fails midway

The deferred reload is what makes a partial apply safe. Entities activate in the database while the *running* engine is still serving the previous estate, so until phase 4 live traffic is unaffected:

| Fails during | Target state | Live traffic | Recovery |
|---|---|---|---|
| **1 — receipt claim** | Nothing written | Unaffected | Fix the cause and re-run. A reused applied version needs a version bump. |
| **2 — staging** | Some entities have new **draft** versions; nothing activated | Unaffected; drafts serve nothing | Fix the artifact and re-run `apply`. A staged receipt may be re-claimed. |
| **3 — activation** | Entities before the failure are active in the database; those after are still drafts. **The engine has not been reloaded** | Unaffected; the old engine is still serving | Fix the cause and re-run `apply` (it is idempotent), or `POST /engine/reload` to serve what did activate |
| **4 — reload** | Every entity is active in the database | Unaffected until a reload happens | `POST /api/v1/admin/engine/reload` |
| **5 — receipt flip** | The estate is live and correct; the receipt still reads `staged` | Correct | Re-run `apply`; it converges the receipt |

In every case the receipt stays `staged`, which is what makes a corrected re-run at the same version legal. Only an **applied** version is content-immutable.

> [!TIP]
> A failed apply that you cannot fix at once is not an emergency: nothing is serving the half-applied estate. Leave it staged, fix the artifact, and re-run.

## Apply at startup instead

An image that carries its definitions does not need a deploy step to apply them. Compile the artifact at build time and name it in [`[packages] apply`](../../reference/configuration/packages.md). The node runs this same apply on itself at startup, through its own admin routes, before `/readyz` reports it ready. A restart is a no-op, and a failure exits the process.

## Retire what a package no longer carries

Removing a channel from the definitions does not remove it from a target, because `apply` only adds and updates. `--prune` removes what the package's current version carried and the new one does not. Preview it with `plan`, then apply it:

```bash
orion-server package plan  -s https://prod.orion.internal -f payments-1.5.0.json --prune
orion-server package apply -s https://prod.orion.internal -f payments-1.5.0.json --prune
```

Removed channels are archived before activation, so a route can move to a new channel id in one apply. Workflows, plugins, models and connectors go after activation. Every removal lands in the same single reload. An entity another package now carries is kept. A workflow or connector something outside the package still uses stops the apply before anything is written. `--prune=delete` deletes instead of archiving. The rules are in [Prune what a version dropped](../../reference/cli/orion-server/package.md#prune-what-a-version-dropped).

Pass `--prune` on every apply. It measures from the version being replaced, so an entity an apply without it left behind is not seen again.

## Roll back

Re-apply the previous artifact version:

```bash
orion-server package apply -s https://prod.orion.internal -f payments-1.3.0.json
```

`apply` sees that `1.3.0` is applied here but that `1.4.0` superseded it. It therefore stages and activates `1.3.0`'s content again rather than stopping at the receipt, and `plan` reports the same verdict first. Entities roll *forward* carrying the older content; nothing moves backward, and the receipt history records both moves. With `--prune`, the rollback also archives what only `1.4.0` carried. That is the whole rollback procedure. There is no separate command, because a rollback is a promotion of something you already shipped. Keep the artifacts. A rollback you cannot perform is a rollback you do not have, and the artifact file is the only thing needed to perform one.

## Verify

Confirm the target runs what you shipped, and read its receipts:

```bash
orion-server package diff -s https://prod.orion.internal -f payments-1.4.0.json
orion-cli packages list
orion-cli packages get payments    # current applied version + history
```

`diff` prints `no drift` and exits `0`. Receipts are what enforce immutability and what make rollback mechanical. They are also the answer to "what is running here", which a database dump cannot give you as directly.

## Sign at deploy time

A target whose `[plugins.trust]` or `[models.trust]` names keys refuses an unsigned plugin or model. The key belongs to the deployment, so sign where the key is and attach the signatures at apply:

```bash
orion-server plugin sign definitions/ --key /run/secrets/signer.pem -o sigs/
orion-server package apply -s https://prod.orion.internal -f payments-1.4.0.json --signatures sigs/
```

The artifact is not changed and its hash does not move. Rotating the key is the same apply with the new signatures. See [Signatures at deploy time](../../reference/cli/orion-server/package.md#signatures-at-deploy-time).

## Secrets survive the trip, if authored as references

Connector exports are masked, which is what makes them safe to commit. It also decides how a connector must be authored to be promotable at all:

| Authored as | Exports as | Re-imports? |
|---|---|---|
| `"token": "env://STRIPE_KEY"` | `"env://STRIPE_KEY"` | **Yes**: a reference names a variable; it is not itself a credential |
| `"token": "sk_live_..."` | `"******"` | **No**: the import is refused |

The refusal is deliberate. Importing `******` would store it as a real credential and fail at the first request. It fails here instead, where you are looking at the file. `lint` treats an `env://` reference that is unset on the machine running it as a warning rather than an error. A CI runner needs no production secrets to check a bundle.

## Promote without packages

To move selected workflows or channels between instances without the package machinery, the per-kind endpoints take the same tags:

```bash
curl -s "$ORION/api/v1/admin/workflows/export?tag=payments" | jq '.data' > workflows.json
curl -s -X POST "$ORION/api/v1/admin/workflows/import?dry_run=true" \
  -H 'Content-Type: application/json' --data @workflows.json
```

Each `/export` emits exactly what its `/import` accepts, and each export reads inside one repeatable-read transaction, so the snapshot is consistent. You give up the closure computation, the receipt, and the single-reload apply. The `on_conflict` modes that govern importing over an existing estate are specified in [Promoting over an existing estate](../../reference/admin-api/export-and-promotion.md#promoting-over-an-existing-estate-on_conflict).

## Next steps

- [Packages](../../concepts/packages.md): what a package is and why the boundary sits there.
- [Test and promote a service](../../get-started/tutorials/test-and-promote.md): the whole flow against two local instances.
- [Audit logs](../run/audit-logs.md): how a promotion appears in the trail.
- [CLI › package](../../reference/cli/orion-server/package.md): every flag of every verb.
