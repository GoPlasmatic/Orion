<!-- description: GET /api/v1/admin/functions serves the live function catalogue: every name with its category, source, aliases, retry safety and input schema. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Runtime function discovery

`GET /api/v1/admin/functions` returns the live catalogue of task functions a running instance accepts, with the input schema of every function Orion declares one for. Tooling and assistants read it instead of this site, so generated workflows use the field names the instance checks.

## Synopsis

```bash
curl -s http://localhost:8080/api/v1/admin/functions \
  -H "Authorization: Bearer $ORION_ADMIN_KEY"
```

## Description

The catalogue serves every function on the [Task functions](./index.md) hub. A function contributed by the engine carries `source: "engine"` and no `input_fields`. Orion declares no schema for it and does not check its input when a workflow is saved. An Orion handler carries `source: "orion"` and its schema. `validation` carries `validate` in `aliases` rather than appearing twice.

The functions of every active [plugin](../plugin-manifest.md) appear in the same catalogue with `source: "plugin"` and a `plugin` block naming the plugin id, version and component digest. Their field tables come from the plugin's manifest rather than this site. The vocabulary is the same (`kind`, `required`, `resolvable`, `template_at`), and workflow validation reads it the same way.

The [Orion agent skill](../../guides/ai/agent-skill.md) points an assistant at the same endpoint.

## Response

One entry per function:

| Field | Type | Description |
|---|---|---|
| `name` | string | The name a task writes in `function.name` |
| `category` | string | `connector`, `control`, `data`, `compute` or `utility`; the wire value, not the grouping on the hub |
| `source` | string | `engine`, `orion` or `plugin` |
| `aliases` | array | Other accepted spellings; `validation` lists `validate` |
| `retry_safety` | string | The answer on the [Retry safety](./retry-safety.md) table, per function |
| `input_fields` | array | The field table an Orion handler validates against; absent for an engine built-in |
| `plugin` | object | For a plugin function: the plugin id, version and component digest |

## Compatibility

**Since:** Orion 1.2 for the complete catalogue. Earlier releases served only the functions with a declared schema.

## Related

- [Task functions](./index.md): the same catalogue, as pages.
- [Admin API › Functions](../admin-api/functions.md): the endpoint in its API context.
- [Plugin manifest and ABI](../plugin-manifest.md): where a plugin function's field table comes from.
- [Use the agent skill](../../guides/ai/agent-skill.md): pointing an assistant at the live schemas.
