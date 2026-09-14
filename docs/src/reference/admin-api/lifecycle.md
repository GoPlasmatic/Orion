<!-- description: How an admin write moves an entity through draft, active and archived, which transitions reload the engine, and what a version is. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Lifecycle over the API

What an admin write does to an entity's status and version, and which writes reach the running engine.

Channels, workflows, plugins and models all follow the same **draft → active → archived** lifecycle, enforced by database triggers rather than by convention:

1. **Create:** entities are created as `draft` (not loaded into the engine)
2. **Update:** only draft versions can be updated through `PUT`
3. **Activate:** `PATCH /status` with `{"status": "active"}` loads the entity into the engine
4. **New version:** `POST /versions` creates a new draft version from the active entity
5. **Archive:** `PATCH /status` with `{"status": "archived"}` removes from the engine

A channel links to a workflow through `workflow_id`. Activating a channel makes it available for data processing; activating a workflow makes its logic available to the engine. Activating a plugin registers its task functions, and activating a model makes it available to `model_infer`. A model may only be activated once its [admission verdict](./models.md) is `passed`.

Connectors are the exception: they are not versioned, have no draft and no `activate`, and `PUT` writes in place.

Activation order is enforced, not conventional.

A workflow is refused at create and update time when it names a task function the engine does not serve. That includes a plugin function whose plugin is not yet active and loaded. A workflow refuses to activate while a connector its tasks reference is missing or of the wrong type. A channel refuses to activate while its `workflow_id` is unset, names a workflow that does not exist, or names one with no active version. The working order for a bundle is therefore plugins → connectors → workflows → channels. That is the order `?dry_run=true` lets you verify before writing anything.

Models are not gated this way. A workflow naming a model that is absent or inactive is accepted and activated. The channels reaching it are [quarantined](../../operate/run/monitoring.md) on any node that cannot serve the model. Activate models before the workflows that name them — between connectors and workflows, since a model's artifact is fetched through a `storage` connector.

**Channel names are unique**: the data plane and `channel_call` address
channels by name, so a name may belong to only one `channel_id`. Create, update and import answer `409` for a name another channel already holds, compared against every channel's current version. Activation also refuses a name another *active* channel holds, which covers rows created before this rule existed. `orion-server preflight` reports pre-1.0 duplicates before an upgrade.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [The entity lifecycle](../../concepts/lifecycle.md): the concept these endpoints enforce.
- [Status changes](./status-changes.md): the endpoint that moves an entity between statuses.
- [Version a service](../../guides/author/versioning.md): the same rules, as a walkthrough.
