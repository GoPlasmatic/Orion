<!-- description: A connector's name, config, enabled flag and tags; how a read gives the config back twice, and what makes the parsed copy null. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Definition and identity

The fields every connector row carries, and the two masked copies of `config` a read gives back.

You create a connector through the [Admin API](../admin-api/connectors.md):

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" }
  },
  "enabled": true,
  "tags": ["payments"]
}
```

- **`name`**: required, at most 255 characters. Workflows and channel stores reference the connector by this name. Connectors are unversioned — an update replaces the stored config.
- **`config`**: the per-type object documented below. Its `type` field selects the shape.
- **`enabled`**: defaults to `true`. A disabled connector is never loaded; export → import preserves the flag ([endpoints](../admin-api/connectors.md)).
- **`tags`**: selection labels for `?tag=` filtering and [package export](../../operate/maintain/promotion.md).

A read gives the config back **twice**, both copies masked:

| Field | Type | Use |
|---|---|---|
| `config` | object | The shape `POST` and `PUT` accept, so a read response can be edited and written straight back. Read this one. |
| `config_json` | string | The stored document verbatim, as a string. Kept for the life of the 1.x line; a client reading it has to parse the string before it can write it back. |

They are the same document — `config` is parsed *from* the masked string, so it
cannot carry a secret the string form has already replaced. `config` is `null`
only when the stored document no longer parses, the same condition that empties
`content_hash`.

> [!NOTE]
> Connector configs ignore unknown top-level fields, so rows written by older versions keep loading. The `operations`, `retry`, and `dialect` blocks are the exception: each refuses unknown keys, as its section states. A misspelled control would otherwise read as protection while providing none.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Admin API — Connectors](../admin-api/connectors.md): the endpoints, and the reachability probe.
- [Secrets by reference](./secrets.md): the `env://` and `vault://` schemes a config field may hold.
- [Secret masking](./masking.md): what a read gives back readable, and what it replaces.
