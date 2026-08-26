<!-- description: Orion generates an OpenAPI 3.1 document covering every HTTP endpoint, request body and response schema — the exact contract behind the API pages. -->
# OpenAPI Specification

Orion generates an OpenAPI 3.1 document covering every HTTP endpoint, request
body, and response schema. It is the exact contract; the hand-written API pages
summarize the semantics around it.

## Getting the spec

- **Running server:** `GET /api/v1/openapi.json` returns the spec for the exact
  binary you are running.
- **Offline:** [`orion-server dump-openapi`](./cli.md) prints the same document
  without starting a server.
- **Repository snapshot:** a copy is committed at
  [`docs/openapi.json`](https://github.com/GoPlasmatic/Orion/blob/main/docs/openapi.json).
  It is regenerated on API changes and is not part of this book.

This book cannot render the spec inline. Open the snapshot on GitHub, or load
any of the three copies into an OpenAPI viewer.

## Swagger UI

A running server serves interactive documentation at `/docs`, backed by the
same generated document.

## Gating

`server.docs.enabled` controls both `/docs` and `/api/v1/openapi.json`:

| Setting | Effect |
|---------|--------|
| unset | Served only when `environment` is not a production variant |
| `true` | Served everywhere, including production |
| `false` | Never served |

When disabled, the routes are not registered. Both paths return `404`, never
`401`, so their existence is not advertised.

> [!NOTE]
> The spec describes the whole admin surface anonymously. Production
> deployments keep it off by default.

## What lives where

- **The spec:** every path, parameter, request body, and response schema.
- **The API pages:** semantics and cross-endpoint rules — lifecycles, conflict
  handling, versioning — in [Admin API](./admin-api.md) and
  [Data API](./data-api.md).

When the two disagree on a shape, the spec dumped from your binary wins.

## Related

- [Admin API](./admin-api.md) — semantics of the management endpoints the spec
  only shapes.
- [Data API](./data-api.md) — how requests reach channels, and the trace
  endpoints.
- [CLI](./cli.md) — `dump-openapi` and the other diagnostic subcommands.
- [Configuration](./configuration.md#api-docs) — the `server.docs.enabled`
  setting.
