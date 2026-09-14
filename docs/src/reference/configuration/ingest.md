<!-- description: The ingest.max_payload_size setting: the data-plane request body bound, separate from the admin plane's own limit, with its default and override. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Ingest settings

The `[ingest]` section: the one bound on a data-plane request body.

## Synopsis

```toml
[ingest]
max_payload_size = 1048576
```

## Description

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `ingest.max_payload_size` | `1048576` | `ORION_INGEST__MAX_PAYLOAD_SIZE` | Raise for large request bodies on the **data plane**. The admin API has its own bound (`server.max_admin_body_size`), so raising this one does not widen the unauthenticated surface. |

## Options

## Related

- [Server settings](./server.md): the admin plane's own body bound.
- [Data API](../data-api.md): the request path this bounds.
- [Production checklist](../../operate/production-checklist.md): sizing the limits before traffic.
- [Server configuration](./index.md): every section, by what you are configuring.
