<!-- description: The two engine endpoints: reading the running engine's status, and hot-reloading channels and workflows without restarting the process. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Engine endpoints

Reading the running engine, and reloading it.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/engine/status` | Engine status: version, uptime, workflow counts, channels, the generation served, its load issues and this node's capabilities |
| POST | `/api/v1/admin/engine/reload` | Hot-reload channels and workflows, answering with the generation published and its load issues |

## What a generation could not load

A reload does not fail when one entity does not load. The entity is quarantined and everything else serves, so a `200` from a reload is not proof that what you activated is serving. Both endpoints say what the generation refused:

```json
{
  "data": {
    "reloaded": true,
    "workflows_count": 12,
    "generation": 41,
    "load_issues": {
      "channels": [
        { "channel": "nightly-sweep", "channel_id": "nightly-sweep", "workflow_id": "nightly",
          "reason": "cron scheduler disabled on this node (cron.enabled = false): the schedule would never fire" }
      ],
      "plugins": [],
      "models": [],
      "connectors": []
    }
  }
}
```

The reload's answer describes the generation *that reload* published. `GET /engine/status` describes the one this node serves now, and adds `capabilities`: whether this node runs cron channels, plugins and models. The four lists are the ones an admin sees on `/health`, built by the same code, so the surfaces cannot disagree. Tooling that needs them should read this endpoint rather than scrape `/health`, whose detail depends on auth configuration.

Both answers describe the node that answered. Peers reload on the epoch bump and may refuse differently, for example with a different `cron.enabled`.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Status changes](./status-changes.md): the `reload=defer` batches this commits.
- [Engine settings](../configuration/engine.md): the bounds the engine is built with.
- [How Orion works](../../concepts/how-orion-works.md): what a reload republishes.
