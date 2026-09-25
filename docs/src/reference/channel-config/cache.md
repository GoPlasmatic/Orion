<!-- description: The cache block of a channel: serving repeated identical sync requests from a stored response, the cache key, key_logic, TTL and the backing store. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `cache`

`cache` serves repeated identical requests from a stored response instead of executing the workflow. It applies to the synchronous HTTP ingress only.

## Synopsis

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `enabled` | boolean | yes | — | `false` disables the cache without removing the block. |
| `ttl_secs` | integer | no | `300` | Seconds an entry lives. |
| `cache_key_fields` | array of strings | no | whole payload | Payload fields that form the cache key. |
| `key_logic` | JSONLogic | no | — | Computes the cache key over `{data, metadata}`. Takes precedence over `cache_key_fields`. |
| `connector` | string | no | in-memory | Name of a [cache connector](../connectors/index.md) backing the cache. In cluster mode the default is the shared cluster Redis. |

**The cache key** is derived from exactly these parts: the channel name, the HTTP method, the route parameters, the query string, and the request payload. Parameters and query string are order-independent. The payload part is the whole payload, the subset named by `cache_key_fields`, or the result of `key_logic`.

`key_logic` is the general form. It reads `{"data": …, "metadata": …}`, the same metadata `validation_logic` reads, and **replaces** the payload-derived half of the key rather than adding to it. An expression that says what varies the response is a complete answer. Mixing it with a payload hash would put back the fields it was written to exclude. When both are declared, `key_logic` decides the key and `cache_key_fields` is ignored.

A per-user cache keys on the verified subject. On a [`jwt`](./auth.md) channel the claims are at `metadata.auth.claims`, and a caller cannot supply that key, so a request without a verified token resolves the expression to `null` and bypasses the cache:

```json
"cache": {
  "enabled": true,
  "ttl_secs": 60,
  "key_logic": { "cat": [{ "var": "metadata.auth.claims.sub" }, "|", { "var": "data.report_id" }] }
}
```

An expression that does not compile quarantines the channel rather than falling back. A cache key that silently widens serves one caller's body to the next. One that resolves to `null` at request time bypasses the cache for that request, as an unresolvable `cache_key_fields` does. Each entry resolves as a literal payload key (`user_id`), a dotted path (`user.id`), or the same path with a leading `data.` prefix (`data.user_id`). A request that resolves **none** of the declared fields bypasses the cache entirely. The workflow runs, nothing is stored, and Orion logs a warning naming the channel and fields. That almost always means the names do not match the payload shape.

> [!WARNING]
> Request headers are not part of the cache key unless `key_logic` reads them from `metadata.headers`. Without that, a cached entry is shared by every caller whose method, route, query, and payload agree, whatever headers they sent. Credential headers are masked there, so a response that varies by caller must key on the verified claims, not on a token.

Behaviour:

- A hit is served without executing the workflow and without consuming a backpressure permit.
- A replayed idempotency key answers `409` before the cache is consulted.
- The response is stored on success and expires after `ttl_secs`.
- A cached [shaped](./response.md) response replays its status and headers, not only its body.
- A write-gated cache connector is refused for the response cache; see [operation gates](../connectors/index.md).

**Cluster mode.** With the shared cluster Redis, hits are shared across replicas. A channel whose cache connector is missing, broken, or explicitly in-memory refuses to load. On a single node it falls back to process memory with a warning.

## Related

- [Connector types › Cache](../connectors/cache.md): the connector that backs the cache.
- [Configure a channel](../../guides/author/channels.md): adding a cache in practice.
- [`rate_limit`](./rate_limit.md): the same `key_logic` vocabulary.
- [Deploy a cluster](../../operate/deploy/cluster.md): the shared cache a cluster requires.
- [Channel configuration](./index.md): every key, with its page.
