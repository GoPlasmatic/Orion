<!-- description: Active Orion workflows are immutable. How to cut a new version, roll it out to a share of traffic, and roll back to content guaranteed to be what last served. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Version and roll out changes

Active workflows and channels are immutable, so changing one means creating a new version and activating it. That is also what makes rollback trustworthy: the old content is guaranteed to be exactly what it was when it last served. This guide is the mechanics, [rolling back](#roll-back) included.

## Before you start

You need an active workflow to change, `orion-cli` pointed at the instance, and admin access to it. The rules behind every step are in [The entity lifecycle](../../concepts/lifecycle.md).

## Create a new version

Cut a draft from the active version, edit it, and list the history:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/order-processing/versions

curl -s -X PUT http://localhost:8080/api/v1/admin/workflows/order-processing \
  -H 'Content-Type: application/json' --data @workflow.json

curl -s http://localhost:8080/api/v1/admin/workflows/order-processing/versions
```

</section>
<section data-tab="CLI">

```bash
orion-cli workflows new-version order-processing
orion-cli workflows update order-processing -f workflow.json
orion-cli workflows versions order-processing
```

</section>
</div>

Only drafts accept updates, and only one draft per id exists at a time, so "the draft" is never ambiguous. Creating and editing drafts does not touch the running engine.

## Check an activation before you make it

Ask whether the activation would succeed, without writing anything:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/order-processing/status?dry_run=true" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

</section>
<section data-tab="CLI">

```bash
orion-cli workflows activate order-processing --dry-run
orion-cli channels activate orders --dry-run
```

</section>
</div>

It is worth it for channels especially, whose activation requires an active workflow and whose stored config must still build. The findings come back as `{valid, errors, warnings}` in a `200`, not as a failure status, because a plan wants the report rather than the first error. The CLI turns that into an exit code, `0` when the transition would succeed and `1` when it would be refused. `--dry-run` therefore gates a script directly.

## Activate

Change the status:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

</section>
<section data-tab="CLI">

```bash
orion-cli workflows activate order-processing
```

</section>
</div>

The engine rebuilds and swaps atomically; in-flight requests finish on the engine they started with. In cluster mode the change reaches every replica through the shared config epoch, so you activate once rather than per node.

## Batch several activations into one reload

Mark each change without rebuilding, then reload once:

<div class="tabs">
<section data-tab="curl">

```bash
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/a/status?reload=defer" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
curl -s -X PATCH "http://localhost:8080/api/v1/admin/workflows/b/status?reload=defer" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
curl -s -X POST  http://localhost:8080/api/v1/admin/engine/reload
```

</section>
<section data-tab="CLI">

```bash
orion-cli workflows activate order-processing --defer-reload
orion-cli channels activate orders --defer-reload
orion-cli engine reload
```

</section>
</div>

Use it when several entities must go live together: a workflow and the channel that points at it, or a whole service. Nothing serves the new versions until the reload, so a half-applied set never takes traffic. `orion-cli workflows rollout <id> -p <n> --defer-reload` takes the same flag, because a rollout change would otherwise rebuild the engine on its own. This is what `orion-server package apply` does on your behalf; see [Promote between environments](../../operate/maintain/promotion.md).

## Roll out gradually

Activate a new version at a percentage instead of all at once, ramp it, then promote it fully:

```bash
# 10% of traffic to the new version
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status": "active", "rollout_percentage": 10}'

# Ramp
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/rollout \
  -H 'Content-Type: application/json' -d '{"rollout_percentage": 50}'

# Promote fully; this archives the previously active version
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/rollout \
  -H 'Content-Type: application/json' -d '{"rollout_percentage": 100}'
```

The remainder keeps going to the previously active version.

The split is sticky per caller. The bucket is a hash of a stable caller identity. The same caller lands on the same version on every request and on every replica. A user does not flip between versions mid-session. The identity is the header named by `engine.rollout_sticky_header` when set, else the forwarded client IP:

```toml
[engine]
rollout_sticky_header = "x-user-id"
```

A direct connection with neither falls back to a random per-request bucket. That still honours the percentages in aggregate but is not sticky for that caller.

## Roll back

Rolling back is rolling *forward* to the old content. There is no command that reactivates an archived version in place. `PATCH /{id}/status` addresses a workflow id, not a version, and activating always promotes the current draft. What you do instead is put the known-good content into a new draft and activate that. Because active versions are immutable, the content you are copying is guaranteed to be exactly what it was when it last served.

If you promote with packages, re-apply the previous artifact and stop reading; that is the whole procedure, and [Promote between environments](../../operate/maintain/promotion.md#roll-back) covers it:

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

# 4. Activate it; the bad version is archived as this one goes live
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/order-processing/status \
  -H 'Content-Type: application/json' -d '{"status": "active"}'
```

Two things that look like shortcuts are not. Setting a rollout to `0` is refused. `PATCH /{id}/rollout` accepts `1` to `100`, because a version serving no traffic is an archived version, not an active one. And archiving the bad version does not fall back to its predecessor. Archiving takes *every* active version of that workflow out of service, so any channel bound to it is quarantined and starts answering `503`. Roll forward instead.

## Move an estate between instances

The per-kind endpoints export and import plain JSON:

```bash
# Snapshot into version control
curl -s "http://localhost:8080/api/v1/admin/workflows/export?status=active" | jq '.data' > workflows.json

# Preview the import; writes nothing
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import?dry_run=true" \
  -H 'Content-Type: application/json' --data @workflows.json

# Import, as drafts
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import" \
  -H 'Content-Type: application/json' --data @workflows.json
```

Each `/export` emits exactly what its `/import` accepts. `?tag=` and `?status=` narrow the set.

By default an import is create-only and a collision is an error. `?on_conflict=skip` leaves existing entities alone; `?on_conflict=new_version` cuts a new draft version carrying the imported content. Under either of those two modes, re-importing an unmodified export reports `unchanged` for everything, which is what makes the import safe to retry from CI:

```bash
curl -s -X POST "http://localhost:8080/api/v1/admin/workflows/import?on_conflict=new_version" \
  -H 'Content-Type: application/json' --data @workflows.json
```

The full matrix is in [Promoting over an existing estate](../../reference/admin-api/export-and-promotion.md#promoting-over-an-existing-estate-on_conflict).

> [!TIP]
> For a whole service, prefer `orion-server package` over these endpoints. It computes the closure, activates in dependency order, reloads once, and leaves a receipt. See [Promote between environments](../../operate/maintain/promotion.md).

## Verify

Confirm which version serves, and that the engine reloaded:

```bash
orion-cli workflows versions order-processing
orion-cli engine status
```

The versions list shows exactly one `active` row, or two while a rollout is in progress, with the percentage beside the newer one. `engine status` reports the generation the last activation published.

## Next steps

- [The entity lifecycle](../../concepts/lifecycle.md): the rules behind all of this, in one page.
- [Admin API](../../reference/admin-api/index.md): the endpoints and their parameters.
- [Promote between environments](../../operate/maintain/promotion.md): the packaged form of the same job.
- [Monitor and alert](../../operate/run/monitoring.md): watching a rollout while it ramps.
