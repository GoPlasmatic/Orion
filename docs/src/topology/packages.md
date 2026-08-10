# Packages & Promotion

One Orion instance runs many services side by side — a **modular monolith**.
The module boundary is the **package**: the channels, workflows, and
connectors that make up one service, named and versioned as a unit so it can
be exported from one instance and imported into another — dev to QA to
production, or from a shared template instance into every region that runs it.

The [three primitives](../concepts/how-orion-works.md#three-primitives) compose a
service; the package is how a service *ships*. Each package deploys, promotes,
and rolls back independently, while every package shares one runtime, one
database, and one operational surface.

```orion-diagram
{
  "direction": "LR",
  "groups": [
    { "id": "dev", "label": "DEV instance" },
    { "id": "prod", "label": "PROD instance" }
  ],
  "nodes": [
    { "id": "TAGGED", "label": "Tagged entities", "sublabel": "channels · workflows · connectors\ntags: [\"pkg:payments\"]", "type": "accent", "group": "dev" },
    { "id": "ARTIFACT", "label": "📦 payments-1.4.0.json", "sublabel": "package artifact\nname · version · content_hash", "type": "infra" },
    { "id": "APPLIED", "label": "Applied package", "sublabel": "staged → activated → receipt", "type": "accent", "group": "prod" }
  ],
  "edges": [
    { "from": "TAGGED", "to": "ARTIFACT", "label": "package export" },
    { "from": "ARTIFACT", "to": "APPLIED", "label": "lint → plan → apply" }
  ]
}
```

## What a package is

A package exists in two forms:

- **Source form** — a directory of entity JSON files, one per channel,
  workflow, and connector, the way the
  [example packages](../getting-started/examples.md) ship in the repository.
  This is the form you author, review, and keep in git.
- **Artifact form** — one JSON document produced by `orion-server package
  export`, carrying everything a target instance needs to run the service:

```json
{
  "package": {
    "name": "payments",
    "version": "1.4.0",
    "orion": "1.0.0",
    "content_hash": "sha256:…",
    "exported_from": "https://dev.orion.internal",
    "exported_at": "2026-08-10T12:00:00Z"
  },
  "requires": { "channels": [], "connectors": ["shared-redis"] },
  "connectors": [ … ],
  "workflows":  [ … ],
  "channels":   [ … ]
}
```

The entity arrays hold the exact shapes the `/import` endpoints accept, so
nothing is reshaped between export and apply. `content_hash` is computed over
the entities' *importable content* — DB-owned fields (`version`, `status`,
timestamps) are excluded — so the same logic always hashes the same, whichever
instance exported it.

`requires` declares the package's **boundaries**: names the package uses but
deliberately does not contain (a shared cache connector owned by the platform
team, a `channel_call` target owned by another package). Declared requirements
keep closures small; `plan` verifies each one exists and is active on the
target before anything is written.

## Selecting what ships

Membership is declared with **tags**. Give every entity of a service the same
`pkg:` label when you create it:

```json
{ "channel_id": "payments", "tags": ["pkg:payments"], … }
```

Export then selects **channels** — by tag or by explicit id — and computes the
closure from there: each selected channel brings its workflow, and each
workflow brings every connector it references (via
`GET /workflows/{id}/dependencies`):

```bash
export ORION_ADMIN_TOKEN=…   # sent as the admin bearer token

# Everything tagged pkg:payments, plus its workflow/connector closure
orion-server package export -s https://dev.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json

# …or hand-pick channels by id
orion-server package export -s https://dev.orion.internal \
  --channels payments,payment-refunds --name payments --version 1.4.0 \
  -o payments-1.4.0.json
```

The channel is the unit of selection because each channel names exactly one
workflow — selecting the endpoints selects the service. A `channel_call`
target outside the selection is not pulled in; it lands in `requires`, on the
theory that a channel you didn't select belongs to somebody else's package.

> **Moving a subset without packaging.** To move selected workflows or
> channels between instances *without* the package machinery, the per-kind
> endpoints take the same tags: `GET /workflows/export?tag=…` emits exactly
> what `POST /workflows/import` accepts, with `?status=` to narrow further.
> You give up the closure computation, the receipt, and the single-reload
> apply — see [Admin API › Export & Promotion](../reference/admin-api.md#export--promotion)
> for that flow and its `on_conflict` modes.

## The promotion flow

Five verbs, one artifact, no shared storage between instances — the artifact
file is the only thing that travels:

```bash
orion-server package lint  -f payments-1.4.0.json           # offline: shapes, closure, hash
orion-server package plan  -s https://qa.orion.internal  -f payments-1.4.0.json
orion-server package apply -s https://qa.orion.internal  -f payments-1.4.0.json
orion-server package diff  -s https://prod.orion.internal -f payments-1.4.0.json
```

| Verb | Needs | Writes | What it does |
|------|-------|--------|--------------|
| `export` | source instance | nothing | Capture selected channels + closure into one versioned artifact |
| `lint` | nothing — fully offline | nothing | Validate entity shapes (the same validators the POST endpoints run), closure completeness against `requires`, and the content hash — the CI gate that needs no server and no secrets |
| `plan` | target instance | **nothing** | Pre-flight: receipt immutability check, the exact per-entity action apply would take, `requires` verification, every activation gate |
| `apply` | target instance | everything | Claim the receipt as `staged`, import all entities (`on_conflict=new_version`), activate in dependency order (connectors → workflows → channels) with `reload=defer`, reload the engine **once**, flip the receipt to `applied` |
| `diff` | any instance | nothing | Compare the instance's content hashes against the artifact's; exits non-zero on drift |

Properties worth relying on:

- **Idempotent.** Re-applying an identical artifact is a no-op — every entity
  reports `unchanged`, which makes the apply safe to retry from CI.
- **Immutable versions.** A changed artifact reusing an *applied* version is
  refused with a `409` — content changes ride a version bump. A failed apply
  leaves the receipt `staged`, so a corrected re-run at the same version is
  legal.
- **One reload.** However many entities the package carries, the running
  engine rebuilds once, and in cluster mode the config epoch bumps once —
  every replica converges on the whole package, not on a half-applied one.
- **Audited as one operation.** Every call is stamped with
  `X-Orion-Change-Context: package=<name>@<version>`, so the audit trail
  filters back into the promotion that caused it.

## Version receipts on the target

The target instance remembers what was applied — one **receipt** per package
version, inspectable over the [Packages API](../reference/admin-api.md#packages) or the
CLI:

```bash
orion-cli packages list
orion-cli packages get payments   # current applied version + history
```

Receipts are what enforce the immutability rule, and what make **rollback**
mechanical: re-apply the previous artifact version and it becomes current
again — entities roll forward carrying the old content, nothing moves
backward, and the receipt history records both moves.

## Secrets survive the trip — if authored as references

Connector exports are masked. A connector authored with a literal credential
exports as `"******"` and is **refused** on import; one authored with an
`env://` reference round-trips cleanly, and the secret lives in each
instance's deployment environment where it belongs:

```json
{ "name": "stripe", "config": { "token": "env://STRIPE_KEY", … } }
```

See [Admin API › Secrets in an exported bundle](../reference/admin-api.md#secrets-in-an-exported-bundle)
for the full rules, including how `lint` treats a reference that is unset on
the machine running it (a warning, not an error — CI needs no production
secrets).

## Try it with the shipped examples

Every [example package](../getting-started/examples.md) in the repository is
tagged `pkg:<name>`, so the whole flow is reproducible with two local servers:

```bash
./examples/deploy.sh high-value-order        # deploy to the first instance

orion-server package export -s http://localhost:8080 \
  --tag pkg:high-value-order --name high-value-order --version 1.0.0 \
  -o high-value-order-1.0.0.json

orion-server package apply -s http://localhost:9090 -f high-value-order-1.0.0.json
curl -X POST http://localhost:9090/api/v1/data/high-value-orders \
  -H 'Content-Type: application/json' \
  --data @examples/packages/high-value-order/request.json
```
