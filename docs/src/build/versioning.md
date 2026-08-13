# Version & Roll Out Changes

Active workflows and channels are immutable. Changing one means creating a new
version and activating it — which is also what makes rollback trustworthy: the
old content is guaranteed to be exactly what it was when it last served. This
page is the mechanics, [rolling back](#roll-back) included.

## Create a new version

```bash
# New draft version from the active one
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/order-processing/versions

# Edit the draft (only drafts accept updates)
curl -s -X PUT http://localhost:8080/api/v1/admin/workflows/order-processing \
  -H 'Content-Type: application/json' --data @workflow.json

# List the history
curl -s http://localhost:8080/api/v1/admin/workflows/order-processing/versions
```

Only **one draft per id** exists at a time, so "the draft" is never ambiguous.
Creating and editing drafts does not touch the running engine.

## Check an activation before you make it

```bash
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/order-processing/status?dry_run=true" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

`?dry_run=true` reports whether the activation would succeed and writes nothing.
Worth it for channels especially, whose activation requires an active workflow
and whose stored config must still build.

## Activate

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

The engine rebuilds and swaps atomically; in-flight requests finish on the
engine they started with. In cluster mode the change reaches every replica
through the shared config epoch, so you activate once rather than per node.

## Batch several activations into one reload

```bash
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/a/status?reload=defer" ...
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/b/status?reload=defer" ...
curl -s -X POST  http://localhost:8080/api/v1/admin/engine/reload
```

`?reload=defer` marks the change without rebuilding. Use it when several
entities must go live together — a workflow and the channel that points at it,
or a whole service. Nothing serves the new versions until the reload, so a
half-applied set never takes traffic.

This is exactly what `orion-server package apply` does on your behalf. See
[Promote Between Environments](../operate/promotion.md).

## Roll out gradually

Activate a new version at a percentage instead of all at once:

```bash
# 10% of traffic to the new version
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status": "active", "rollout_percentage": 10}'

# Ramp
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/rollout \
  -H 'Content-Type: application/json' -d '{"rollout_percentage": 50}'

# Promote fully — this archives the previously active version
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/rollout \
  -H 'Content-Type: application/json' -d '{"rollout_percentage": 100}'
```

The remainder keeps going to the previously active version.

**The split is sticky per caller.** The bucket is a hash of a stable caller
identity, so the same caller lands on the same version on every request and on
every replica — a user does not flip between versions mid-session. The identity
is the header named by `engine.rollout_sticky_header` when set, else the
forwarded client IP:

```toml
[engine]
rollout_sticky_header = "x-user-id"
```

A direct connection with neither falls back to a random per-request bucket,
which still honours the percentages in aggregate but is not sticky for that
caller.

## Roll back

Rolling back is rolling *forward* to the old content. There is no command that
reactivates an archived version in place — `PATCH /{id}/status` addresses a
workflow id, not a version, and activating always promotes the current draft.
What you do instead is put the known-good content into a new draft and activate
that. Because active versions are immutable, the content you are copying is
guaranteed to be exactly what it was when it last served — which is the whole
reason rollback is trustworthy rather than hopeful.

If you promote with packages, re-apply the previous artifact and stop reading —
that is the whole procedure, and [Promote Between
Environments](../operate/promotion.md#roll-back) covers it:

```bash
orion-server package apply -s https://prod.orion.internal -f payments-1.3.0.json
```

Over the admin API directly, it is four calls:

```bash
# 1. Find the version you want back
curl -s http://localhost:8080/api/v1/admin/workflows/order-processing/versions

# 2. Cut a fresh draft from the current version
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/order-processing/versions

# 3. Put the known-good content into that draft
curl -s -X PUT http://localhost:8080/api/v1/admin/workflows/order-processing \
  -H 'Content-Type: application/json' -d @order-processing-v3.json

# 4. Activate it — the bad version is archived as this one goes live
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status": "active"}'
```

Two things that look like shortcuts are not. Setting a rollout to `0` is
refused — `PATCH /{id}/rollout` accepts `1`–`100`, because a version serving no
traffic is an archived version, not an active one. And archiving the bad version
does not fall back to its predecessor: archiving takes *every* active version of
that workflow out of service, so any channel bound to it is quarantined and
starts answering `503`. Roll forward instead.

## Move an estate between instances

The per-kind endpoints export and import plain JSON:

```bash
# Snapshot into version control
curl -s "http://localhost:8080/api/v1/admin/workflows/export?status=active" | jq '.data' > workflows.json

# Preview the import — writes nothing
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import?dry_run=true" \
  -H 'Content-Type: application/json' --data @workflows.json

# Import (as drafts)
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import" \
  -H 'Content-Type: application/json' --data @workflows.json
```

Each `/export` emits exactly what its `/import` accepts. `?tag=` and `?status=`
narrow the set.

By default an import is create-only and a collision is an error.
`?on_conflict=skip` leaves existing entities alone; `?on_conflict=new_version`
cuts a new draft version carrying the imported content. Under either of those
two modes — not the create-only default — re-importing an unmodified export
reports `unchanged` for everything, which is what makes the import safe to retry
from CI:

```bash
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import?on_conflict=new_version" \
  -H 'Content-Type: application/json' --data @workflows.json
```

The full matrix is in
[Admin API](../reference/admin-api.md#promoting-over-an-existing-estate-on_conflict).

> [!TIP]
> For a whole service, prefer `orion-server package` over these endpoints. It
> computes the closure, activates in dependency order, reloads once, and leaves
> a receipt — see [Promote Between Environments](../operate/promotion.md).

## Related

- [The Entity Lifecycle](../concepts/lifecycle.md) — the rules behind all of
  this, in one page.
- [Admin API](../reference/admin-api.md) — the endpoints and their parameters.
- [Promote Between Environments](../operate/promotion.md) — the packaged form of
  the same job.
- [Monitoring & Alerts](../operate/monitoring.md) — watching a rollout while it
  ramps.
