<!-- description: Draft, active, archived — the one-way lifecycle every Orion channel and workflow follows, enforced by the database rather than by convention. -->
# The Entity Lifecycle

Every channel and workflow moves through three states, in one direction. The
rules are enforced by the database, not by convention, which is what makes
AI-generated logic and 3am changes equally safe to accept.

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "draft",    "label": "draft",    "sublabel": "editable · not served", "type": "infra" },
    { "id": "active",   "label": "active",   "sublabel": "served · immutable",    "type": "channel" },
    { "id": "archived", "label": "archived", "sublabel": "retired · replayable",  "type": "datastore", "shape": "rectangle" }
  ],
  "edges": [
    { "from": "draft",  "to": "active",   "label": "activate" },
    { "from": "active", "to": "archived", "label": "archive" }
  ]
}
```

## The three states

- **draft** — editable, and invisible to traffic. Everything you create starts
  here. Only **one draft per id** may exist at a time, so "the draft" is always
  unambiguous.
- **active** — serving, and **immutable**. Nothing can edit an active version;
  changing it means creating a new version and activating that.
- **archived** — retired, and kept. An archived version is a rollback target,
  not a deleted row.

## Why immutability is the load-bearing rule

Because active versions cannot change, every fact about a running service is
fixed once it is serving. A trace names the version that produced it. Rollback
is re-promoting content that is guaranteed to be what it was. A reviewer
approving a diff is approving the exact bytes that will run.

The cost is one extra step: you cannot patch production in place. The benefit is
that no change can arrive without a new version to point at — which is what lets
you hand workflow authoring to an AI assistant and still know what is deployed.

## What moves the engine

The running engine is rebuilt only by transitions that change what should be
served:

| Action | Reloads the engine? |
|---|---|
| Create or update a draft | No |
| Activate, archive, or delete | Yes |
| Change a rollout percentage | Yes |
| Update a connector | No engine rebuild — the connector registry reloads |
| `POST /api/v1/admin/engine/reload` | Yes, on demand |

A reload builds the new engine alongside the old one and swaps it in atomically.
Requests already in flight finish on the engine they started on, so activation
is not a restart and drops no traffic. In cluster mode, the change reaches every
replica through a shared config epoch, so you activate once rather than per
node.

Two options exist for controlling that timing: `?dry_run=true` reports whether a
status change would succeed without making it, and `?reload=defer` applies
several changes before rebuilding once.

## Ordering rules

- **A channel cannot activate ahead of its workflow.** Activation requires the
  named workflow to be active, so an endpoint can never point at logic that is
  not serving.
- **Connectors have no lifecycle.** They are live when saved and replaced when
  updated; there is no draft to activate. Order matters at promotion time for
  that reason: connectors first, then workflows, then channels.
- **A rollout splits versions, not entities.** Activating a new version at less
  than 100% leaves the previous version serving the remainder, bucketed by a
  stable hash of the request.

## When a stored entity cannot be loaded

A channel whose stored config no longer parses — an unknown key, an unset
`env://` reference in its `auth` block — is **quarantined** at load: it is
refused at every ingress rather than served with a guard silently missing. So
is one whose *workflow* cannot be built: a task naming a function the engine
will not dispatch, an input its function cannot parse, or a rollout whose
percentages do not cover the traffic. A channel with nothing runnable behind it
is refused rather than answering with a pipeline that fails every request.

Quarantine is a load-time failure state, not an authentication outcome, and it
clears only when a later reload builds the channel successfully. Run
`orion-server preflight` before an upgrade to find affected channels in
advance, and see [Troubleshooting](../operate/troubleshooting.md) for the full
list of triggers and how to clear one.

## Next steps

- [Version & Roll Out Changes](../build/versioning.md) — the endpoints behind
  each transition, and how to roll one out gradually.
- [Admin API](../reference/admin-api.md) — status changes, versions, and their
  query parameters.
- [Packages](./packages.md) — how a promotion drives these transitions in
  dependency order, with one reload.
- [Your First Service](../getting-started/first-service.md) — the lifecycle in
  four calls.
