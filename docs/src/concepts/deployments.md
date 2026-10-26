<!-- description: A deployment is the one manifest a node serves: content-addressed definitions, connection revisions, a numbered head, plan, apply, rollback and convergence. -->
<!-- type: concept -->
<!-- last_verified: 2026-10-25 -->

# Deployments

A *deployment* is the manifest of everything an Orion node serves, committed as one numbered revision. This page explains its parts, how a change reaches a node, and what a node does when it cannot build what was committed.

## The problem it solves

A runtime that takes one entity at a time drifts. Each channel, workflow and connector changes on its own schedule, so "what is serving now" has no single answer. A rollback means remembering which of twenty rows moved, and two replicas can hold different mixes.

A deployment replaces all of that with one value. The node serves exactly what its newest manifest names. A change is a new manifest, a rollback is an older one, and every node in a cluster builds the same one.

## How it works

You author a *definition set*: a directory of workflow, channel and connector files, with optional shared fragments and plugins. `orion-server compile` checks it and writes a deployment directory. `orion-cli deployments apply -d` uploads that directory and commits it as the next revision. The node then builds a new engine from the revision and swaps it in.

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "set", "label": "Definition set", "sublabel": "your files", "type": "service" },
    { "id": "dir", "label": "Deployment directory", "sublabel": "orion-server compile", "type": "ci" },
    { "id": "head", "label": "Head revision", "sublabel": "deployments apply", "type": "datastore" },
    { "id": "gen", "label": "Serving generation", "sublabel": "every node builds it", "type": "channel" }
  ],
  "edges": [
    { "from": "set", "to": "dir" }, { "from": "dir", "to": "head" }, { "from": "head", "to": "gen" }
  ]
}
```

The diagram shows the path every change takes: files, a compiled directory, a committed head, and the generation each node builds from it. The CLI wraps the admin API's [deployment endpoints](../reference/admin-api/deployments.md), so a script can make the same calls over HTTP.

### Definitions

A *definition* is one workflow, channel or plugin document, stored by its content. The server validates it the way a node loads it, stores its canonical bytes, and names it by their `sha256:` digest. Two spellings of one document are one digest.

A definition carries no id, version or status. The manifest member that names it supplies those. Storing a definition serves nothing.

### Connections

A *connection* is the configuration a built-in driver runs with, such as a PostgreSQL pool or an HTTP client. A connector file in a definition set becomes one. Each `(connection_id, revision)` pair is immutable, and secrets appear in it only as references. [Connectors](./connectors.md) explains the two forms.

### The manifest

A *manifest* lists the members a deployment serves. Each member is an entity, meaning its kind, id and revision, plus the digest of what it serves:

```json
{
  "schema_version": 2,
  "providers": [],
  "members": [
    { "entity": { "kind": "workflow", "id": "order-summary", "revision": 1 },
      "definition_digest": "sha256:87bf52da…" },
    { "entity": { "kind": "channel", "id": "order-summary", "revision": 1 },
      "definition_digest": "sha256:e627767c…",
      "requires": [{ "kind": "workflow", "id": "order-summary", "revision": 1 }] }
  ]
}
```

`requires` is how members depend on each other. A channel requires its workflow, and a workflow requires every connection its tasks name. Every requirement must be a member of the same manifest, so a deployment cannot leave a dependency behind.

### The head

Deployment revisions are numbered from 1, and the newest committed one is the *head*. A node with nothing committed serves nothing. An apply states the head it expects, and fails if another apply committed first. History only moves forward: no revision is edited or deleted.

### Plan, apply and rollback

Three operations move the head, and one only looks:

| Operation | What it does | Commits |
|---|---|---|
| Plan | Checks a manifest against a base revision and lists what it adds and removes | No |
| Apply | Commits the manifest as the next revision, if the head is still the base | Yes |
| Rollback | Commits an earlier revision's manifest again, as a new revision | Yes |

Plan and apply both run the *build check*. They compile the manifest the way a node would build it. A manifest that would not build is refused with `400 DEPLOYMENT_WOULD_NOT_BUILD`, before anything is committed. Typical causes are two channels claiming one route, or a workflow calling a function no member serves.

### Convergence

Every node builds the head on its own. Each node runs one reconcile loop that reads the head and builds the revision it should serve. *Convergence* is the report of which nodes serve a given revision.

A node that cannot build a committed head keeps serving its previous generation. Its `/health` reports `engine_reload` as degraded, and the loop retries on its next tick. This happens when nodes differ, for example one with plugins disabled.

## A minimal example

This definition set is two files, a workflow and the channel that exposes it:

```text
order-summary/
├── workflow.json
└── channel.json
```

Compile it, then apply it to an instance with nothing deployed:

```bash
orion-server compile order-summary -o dist/deployment
orion-cli deployments apply -d dist/deployment
```

The CLI prints the plan before it commits:

```text
Plan: dist/deployment against head revision 0
  + channel order-summary@1
  + workflow order-summary@1
  2 to deploy, 0 unchanged, 0 removed; reviewed manifest digest sha256:7a31fdad…
```

The apply answers `"committed_revision": 1`. Applying the same directory again commits nothing: the CLI reports the head as up to date. [Build your first service](../get-started/tutorials/first-service.md) runs this example end to end.

## The parts at a glance

| Part | Identity | Changes by | Created by |
|---|---|---|---|
| Definition | `sha256:` digest of its canonical bytes | Never; a new document is a new digest | `POST /api/v2/admin/definitions`, or `apply -d` |
| Connection revision | `(connection_id, revision)` | Never; a change is the next revision | `POST /api/v2/admin/connections/{id}/revisions`, or `apply -d` |
| Manifest member | `(kind, id, revision)` | A new deployment naming another revision | `apply -d` assigns revisions, or you write them |
| Deployment revision | A number, from 1 | Never; a change is the next revision | An apply or a rollback |

## Boundaries

- **A deployment replaces; it never merges.** Anything the head serves that the new manifest omits stops serving. Services that must run side by side belong in one definition set.
- **A committed revision does not prove every member serves.** A member a node cannot load is quarantined, and the rest serve. `GET /api/v2/admin/engine/status` lists what the generation refused.
- **A rollback restores configuration, not effects.** Rows already written and requests already sent stay as they are.
- **Revision numbers belong to one instance.** Staging and production assign their own numbers. The digests are what match, because one set compiles to the same bytes everywhere.
- **`compile` deploys one revision per workflow.** A traffic split between two revisions of one workflow is a channel member's `rollouts`, in a manifest you apply with `deployments apply -f`.
- **There is no other write path.** No endpoint edits one entity in place, and no endpoint reloads the engine. A node changes what it serves only by building a committed revision.

## Next steps

- [Build your first service](../get-started/tutorials/first-service.md): write two files, compile them and apply the deployment.
- [Test and deploy a service](../get-started/tutorials/test-and-promote.md): test offline, plan, apply and roll back one revision.
- [Deployment endpoints](../reference/admin-api/deployments.md): every route, field and error behind plan, apply, rollback and convergence.
- [`orion-server compile`](../reference/cli/orion-server/compile.md): what a definition set compiles to, file by file.
- [Run a cluster](../operate/deploy/cluster.md): how every replica converges on one head.
