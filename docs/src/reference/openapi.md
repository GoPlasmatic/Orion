<!-- description: Orion generates an OpenAPI 3.1 document covering every HTTP endpoint, request body and response schema — the exact contract behind the API pages. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# OpenAPI specification

Orion generates an OpenAPI 3.1 document covering every HTTP endpoint, request body and response schema. It is the exact contract; the hand-written API pages summarize the semantics around it. [Admin API](./admin-api/index.md) and [Data API](./data-api.md) are the pages it shapes.

## Synopsis

Three copies of the same document:

| Source | Where | Reflects |
|---|---|---|
| A running server | `GET /api/v1/openapi.json` | The exact binary you are running |
| Offline | [`orion-server dump-openapi`](./cli/orion-server/dump-openapi.md) | The binary on your `PATH`, without starting a server |
| The repository | [`docs/openapi.json`](https://github.com/GoPlasmatic/Orion/blob/main/docs/openapi.json) | The committed snapshot, regenerated on every API change |

This book cannot render the spec inline. Open the snapshot on GitHub, or load any of the three copies into an OpenAPI viewer. A running server also serves Swagger UI at `/docs`, backed by the same generated document.

## Parameters

`server.docs.enabled` controls both `/docs` and `/api/v1/openapi.json`:

| Value | Effect |
|---|---|
| unset | Served only when `environment` is not a production variant |
| `true` | Served everywhere, including production |
| `false` | Never served |

When disabled, the routes are not registered. Both paths return `404`, never `401`, so their existence is not advertised.

## Caveats

- The spec describes the whole admin surface anonymously. Production deployments keep it off by default; see [Close the surfaces you do not need](../operate/run/security.md#close-the-surfaces-you-do-not-need).
- When the API pages and the spec disagree on a shape, the spec dumped from your binary wins. The pages own semantics and cross-endpoint rules: lifecycles, conflict handling, versioning.

## Related

- [Admin API](./admin-api/index.md): the semantics of the management endpoints the spec shapes.
- [Data API](./data-api.md): how requests reach channels, and the trace endpoints.
- [CLI](./cli/index.md): `dump-openapi` and the other diagnostic subcommands.
- [Configuration › API docs](./configuration/server.md#api-docs): the `server.docs.enabled` setting.
