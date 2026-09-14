<!-- description: The status endpoint every entity kind shares: activating and archiving, the dry_run pre-flight, and batching reloads with reload=defer. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Status changes

The one endpoint shape that activates or archives any entity kind, and its two query parameters.

Two query parameters compose with every status and rollout transition:

## Activation pre-flight (`dry_run`)

`PATCH /{kind}/{id}/status?dry_run=true` runs every gate the real transition runs, and answers the `/validate` envelope (`{"data": {"valid", "errors", "warnings"}}`) without writing. The gates are draft existence, connector existence, type and MongoDB `database` (workflows), route collisions and the workflow-active gate (channels), and rollout arithmetic. Gates that the real request fails as a 4xx are reported as `errors` entries in a 200, including "not found". One pass over a whole bundle therefore collects every finding instead of stopping at the first.

## Batching reloads (`reload=defer`)

Every activation, archive and rollout change normally rebuilds the engine and bumps the cluster config epoch. N entities promoted means N full rebuilds on this node, and N resyncs on every peer. `?reload=defer` on the status and rollout endpoints commits the row and records the audit event. It leaves the running configuration untouched **everywhere** until `POST /api/v1/admin/engine/reload`, which rebuilds once and bumps the epoch once. Until that reload, the database and the running engine intentionally disagree — a deferred activation is not serving yet. Tooling that defers must always finish with the explicit reload; an operator making one change omits the parameter.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Lifecycle over the API](./lifecycle.md): the statuses this endpoint moves between.
- [Engine](./engine.md): the reload a deferred batch is committed with.
- [The entity lifecycle](../../concepts/lifecycle.md): the rules it enforces.
