<!-- description: The engine status endpoint: the generation this node serves, what it could not load, and the node's capabilities. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# Engine endpoints

Reading what the running engine on this node serves.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v2/admin/engine/status` | Engine status: version, uptime, workflow counts, channels, the generation served, its load issues and this node's capabilities |

A node builds a new generation each time it applies a committed deployment. There is no reload endpoint. To change what a node serves, apply a new deployment ([Deployment endpoints](./deployments.md#plan-and-apply)).

## What a generation could not load

A generation does not fail when one entity does not load. The entity is quarantined and everything else serves, so a committed deployment is not proof that every member is serving. The status answer says what the generation refused:

```json
{
  "data": {
    "version": "…",
    "workflows_count": 12,
    "generation": 41,
    "load_issues": {
      "channels": [
        { "channel": "nightly-sweep", "channel_id": "nightly-sweep", "workflow_id": "nightly",
          "reason": "cron scheduler disabled on this node (cron.enabled = false): the schedule would never fire" }
      ],
      "plugins": [],
      "connectors": []
    },
    "capabilities": { "cron": false, "plugins": true }
  }
}
```

`capabilities` says whether this node runs cron channels and plugins. The three lists are the ones an admin sees on `/health`, built by the same code, so the surfaces cannot disagree. Tooling that needs them should read this endpoint rather than scrape `/health`, whose detail depends on auth configuration.

The answer describes the node that answered. Peers build the same deployment on their own and may refuse differently, for example with a different `cron.enabled`. [Convergence](./deployments.md#convergence) reports which nodes serve a revision.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Deployment endpoints](./deployments.md): what a node is told to serve.
- [Engine settings](../configuration/engine.md): the bounds the engine is built with.
- [How Orion works](../../concepts/how-orion-works.md): what a generation holds.
