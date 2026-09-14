<!-- description: The `cache` connector config: the Redis or memory backend, key namespace and TTL, and the read and write operation gates. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `cache` connectors

The `config` fields of a `cache` connector, which backs Redis or in-process memory.

Key-value storage for lookups, session state, and temporary data, through [`cache_read` / `cache_write`](../functions/cache_read.md).

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "env://REDIS_URL"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `backend` | string | yes | — | `"redis"` or `"memory"` |
| `url` | string | Redis only | — | Redis connection URL, carrying credentials when needed: `redis://user:pass@host:6379`. Ignored for `"memory"` |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection). Ignored for `"memory"`, which opens no socket |
| `operations` | object | no | all allowed | `read` / `write` — see [Operation gates](./operation-gates.md) |

TTL is set per write, through `cache_write`'s `ttl_secs`. There is no connector-level default.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
