# Promote Between Environments

Promotion moves one service from one instance to another — dev to QA to
production, or a template instance out to every region. The unit is a
[package](../concepts/packages.md): the channels of a service, their workflows,
and the connectors those workflows reference, captured as one versioned
artifact.

Five verbs, one artifact file, and no shared storage between instances. The file
is the only thing that travels.

## Authenticate first

```bash
export ORION_ADMIN_TOKEN=…    # sent as the admin bearer token
```

Every subcommand except `lint` calls an instance's admin API. Against an
instance with `admin_auth.enabled = true` — which production should be — an
unset `ORION_ADMIN_TOKEN` means every call is refused.

`lint` needs no server and no token, which is what lets it run as a CI gate on a
runner holding no production credentials.

## The five verbs

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
| `lint` | Nothing — fully offline | Nothing | Validate entity shapes with the same validators the POST endpoints run, check closure completeness against `requires`, verify the content hash |
| `plan` | Target instance | **Nothing** | Pre-flight: receipt immutability, the exact per-entity action `apply` would take, `requires` verification, every activation gate |
| `apply` | Target instance | Everything | Claim the receipt, stage all entities, activate in dependency order, reload once, flip the receipt to applied |
| `diff` | Any instance | Nothing | Compare the instance's content hashes against the artifact's; exits non-zero on drift |

**Use `lint` as the PR gate. Use `plan` as the pre-deploy gate. Use `diff` as
the post-deploy check** — and as a scheduled job, because it is how you learn
that production drifted from what you shipped.

## Select what ships

Membership is a tag. Give every entity of a service the same label when you
create it:

```json
{ "channel_id": "payments", "tags": ["pkg:payments"], "...": "..." }
```

Export selects **channels** — by tag or by explicit id — and computes the
closure from there: each channel brings its workflow, and each workflow brings
every connector it references.

```bash
# Everything tagged pkg:payments, plus the closure
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json

# …or hand-pick channels by id
orion-server package export -s https://dev.orion.internal \
  --channels payments,payment-refunds --name payments --version 1.4.0 \
  -o payments-1.4.0.json
```

A `channel_call` target outside the selection is **not** pulled in. It lands in
the artifact's `requires` block, on the theory that a channel you did not select
belongs to somebody else's package. `plan` then verifies each requirement exists
and is active on the target before anything is written.

## What `apply` does, in order

Knowing the phases is what lets you interpret a failure, so they are worth
reading once:

1. **Claim the receipt as `staged`.** This is the atomic immutability check — a
   reused applied version with different content is refused here — and it
   doubles as the guard against two applies running at once.
2. **Stage every entity as a draft**, in dependency order: connectors, then
   workflows, then channels. Connector import reloads the connector registry
   server-side, so workflow activation later sees them.
3. **Activate in dependency order, with the reload deferred.** Each activation
   is marked in the database but the engine is not rebuilt yet.
4. **Reload the engine once**, which is also one config-epoch bump in a cluster.
5. **Flip the receipt to `applied`.**

Two properties fall out of that ordering. However many entities the package
carries, the running engine rebuilds **once** — every replica converges on the
whole package, never on a half-applied one. And every call is stamped with
`X-Orion-Change-Context: package=<name>@<version>`, so the
[audit trail](./audit-logs.md) filters back into the promotion that caused it.

## When an apply fails midway

The deferred reload is what makes a partial apply safe: entities activate in the
database while the *running* engine is still serving the previous estate. Until
phase 4, live traffic is unaffected.

| Fails during | Target state | Live traffic | Recovery |
|---|---|---|---|
| **1 — receipt claim** | Nothing written | Unaffected | Fix the cause and re-run. A reused applied version needs a version bump. |
| **2 — staging** | Some entities have new **draft** versions; nothing activated | Unaffected — drafts serve nothing | Fix the artifact and re-run `apply`. A staged receipt may be re-claimed. |
| **3 — activation** | Entities before the failure are active in the database; those after are still drafts. **The engine has not been reloaded** | Unaffected — the old engine is still serving | Fix the cause and re-run `apply` (it is idempotent), or `POST /engine/reload` to serve what did activate |
| **4 — reload** | Every entity is active in the database | Unaffected until a reload happens | `POST /api/v1/admin/engine/reload` |
| **5 — receipt flip** | The estate is live and correct; the receipt still reads `staged` | Correct | Re-run `apply`; it converges the receipt |

In every case the receipt stays `staged`, which is what makes a corrected re-run
at the same version legal. Only an **applied** version is content-immutable.

> [!TIP]
> A failed apply that you cannot immediately fix is not an emergency: nothing is
> serving the half-applied estate. Leave it staged, fix the artifact, and re-run.

## Roll back

Re-apply the previous artifact version:

```bash
orion-server package apply -s https://prod.orion.internal -f payments-1.3.0.json
```

Entities roll *forward* carrying the older content — nothing moves backward —
and the receipt history records both moves. That is the whole rollback
procedure; there is no separate command, because a rollback is just a promotion
of something you already shipped.

Keep the artifacts. A rollback you cannot perform is a rollback you do not have,
and the artifact file is the only thing needed to perform one.

## Inspect receipts on the target

```bash
orion-cli packages list
orion-cli packages get payments    # current applied version + history
```

Receipts are what enforce immutability and what make rollback mechanical. They
are also the answer to "what is actually running here", which a database dump
cannot give you as directly.

## Secrets survive the trip — if authored as references

Connector exports are masked, which is what makes them safe to commit, and which
decides how a connector must be authored to be promotable at all:

| Authored as | Exports as | Re-imports? |
|---|---|---|
| `"token": "env://STRIPE_KEY"` | `"env://STRIPE_KEY"` | **Yes** — a reference names a variable; it is not itself a credential |
| `"token": "sk_live_..."` | `"******"` | **No** — the import is refused |

The refusal is deliberate. Importing `******` would store it as a real
credential and fail at the first request, instead of failing here, where you are
looking at the file.

`lint` treats an `env://` reference that is unset *on the machine running it* as
a warning rather than an error — a CI runner needs no production secrets to
check a bundle.

## Promoting without packages

To move selected workflows or channels between instances without the package
machinery, the per-kind endpoints take the same tags:

```bash
curl -s "$ORION/api/v1/admin/workflows/export?tag=payments" | jq '.data' > workflows.json
curl -s -X POST "$ORION/api/v1/admin/workflows/import?dry_run=true" \
  -H 'Content-Type: application/json' --data @workflows.json
```

Each `/export` emits exactly what its `/import` accepts, and each export reads
inside one repeatable-read transaction, so the snapshot is consistent. You give
up the closure computation, the receipt, and the single-reload apply. The
`on_conflict` modes that govern importing over an existing estate are specified
in
[Admin API › Promoting over an existing estate](../reference/admin-api.md#promoting-over-an-existing-estate-on_conflict).

## Related

- [Packages](../concepts/packages.md) — what a package is and why the boundary
  sits there.
- [Test & Promote a Service](../getting-started/test-and-promote.md) — the whole
  flow against two local instances.
- [Audit Logs](./audit-logs.md) — how a promotion appears in the trail.
- [CLI Reference](../reference/cli.md#package) — every flag of every verb.
