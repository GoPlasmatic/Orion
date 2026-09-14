<!-- description: orion-server dump-openapi prints the public HTTP API's OpenAPI 3.1 document as JSON to stdout, with no config, database or running server. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server dump-openapi`

Prints the public HTTP API's OpenAPI 3.1 spec as JSON to stdout. Needs no config, database, or running server. See [OpenAPI](../../openapi.md).

## Synopsis

```bash
orion-server dump-openapi > openapi.json
```

## Examples

```bash
orion-server dump-openapi > openapi.json
```

## Related

- [OpenAPI specification](../../openapi.md): what the document covers and how it is kept current.
- [Admin API](../../admin-api/index.md): the endpoints the document describes.
- [Data API](../../data-api.md): the runtime request path.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
