<!-- description: The plugin endpoints: uploading a WebAssembly component as JSON or multipart, signatures, versioning, dependants and the schema-compatibility refusal. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Plugin endpoints

The endpoints that upload, version and activate a WebAssembly plugin, and what they refuse.

WebAssembly plugins — sandboxed custom task functions; see [Plugins](../../concepts/plugins.md) and the [Plugins reference](../plugin-manifest.md). The lifecycle is the workflow's. An upload carries the manifest, as TOML text or as a JSON object, plus the component as base64 or a `digest` this instance already holds. The server validates, hashes, compiles and probes it before the draft exists.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/plugins` | Upload a plugin as a draft: `{ "manifest": …, "component": "<base64>", "signature": "<base64>", "tags": [] }`, or the same fields as a `multipart/form-data` form with the component as a raw binary part. `signature` is required when the node's `[plugins.trust]` names keys ([Trust](../plugin-manifest.md#trust)). `400` on a bad manifest, component, digest or signature, or when plugins are disabled on this node |
| GET | `/api/v1/admin/plugins` | List plugins. Filter with `?tag=`, `?status=` |
| GET | `/api/v1/admin/plugins/{id}` | Get the latest version, with this node's load state under `health` |
| PUT | `/api/v1/admin/plugins/{id}` | Update the draft: any of `manifest`, `component`, `digest`, `tags`; an absent field keeps its value |
| DELETE | `/api/v1/admin/plugins/{id}` | Delete every version and any component nothing names. `409` while an active workflow calls one of its functions |
| PATCH | `/api/v1/admin/plugins/{id}/status` | Activate (supersedes the previously active version) or archive (`409` while an active workflow calls a function). `?dry_run=true` / `?reload=defer` — see [Status changes](./status-changes.md) |
| GET | `/api/v1/admin/plugins/{id}/versions` | Version history |
| POST | `/api/v1/admin/plugins/{id}/versions` | New draft version from the latest |
| GET | `/api/v1/admin/plugins/{id}/dependencies` | The functions the latest version declares and the active workflows calling them |
| POST | `/api/v1/admin/plugins/import` | Bulk import (as drafts). Items carry the component inline or name a digest the target holds. `?dry_run=true`, `?on_conflict=fail\|skip\|new_version` |
| GET | `/api/v1/admin/plugins/export` | Export plugins; `?include_artifacts=true` inlines each component as base64 |
| POST | `/api/v1/admin/plugins/validate` | Validate a manifest and component without storing them; `valid: true` means `POST /plugins` would accept the payload on this node |

```bash
# Upload from a manifest and its component
jq -n --rawfile manifest plugin.toml --arg component "$(base64 < component.wasm)" \
  '{manifest: $manifest, component: $component}' \
  | curl -s -X POST http://localhost:8080/api/v1/admin/plugins \
      -H 'Content-Type: application/json' --data @-

# The same upload as a multipart form — no base64, the component streams as bytes
curl -s -X POST http://localhost:8080/api/v1/admin/plugins \
  -F manifest=@plugin.toml -F component=@component.wasm -F tags=codecs

# Activate; the engine reloads and the functions appear in GET /admin/functions
curl -s -X PATCH http://localhost:8080/api/v1/admin/plugins/acme.iso8583/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'

# Export with the components inlined, for a promotion
curl -s "http://localhost:8080/api/v1/admin/plugins/export?include_artifacts=true" | jq '.data' > plugins.json
```

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Plugins](../../concepts/plugins.md): what a plugin is, and what its sandbox may not do.
- [Plugin manifest](../plugin-manifest.md): the `plugin.toml` these endpoints read.
- [Build a plugin](../../guides/extend/plugins.md): the same endpoints, as a walkthrough.
