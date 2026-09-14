<!-- description: The rate_limit block of a channel: the token bucket, the key_logic context and key_headers, cross-ingress semantics, and cluster-wide enforcement. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `rate_limit`

`rate_limit` meters admission with a token bucket: tokens refill at the configured rate, `burst` absorbs short spikes, and an empty bucket answers `429 Too Many Requests`.


## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `requests_per_second` | integer | yes | — | Steady admission rate per bucket. |
| `burst` | integer | no | `requests_per_second / 2 + 1` | Allowance above the steady rate. |
| `key_logic` | JSONLogic | no | caller identity | Expression computing the bucket key. See the context below. |
| `key_headers` | array of strings | no | — | Extra request headers `key_logic` may read, **merged with** the built-in set below. |
| `on_backend_error` | string | no | `"allow"` | `allow` fails open, `deny` refuses with `503`, when the shared cluster Redis cannot answer. Irrelevant on a single node — the in-process limiter cannot fail. |

The limit applies on every ingress, whether or not the platform limiter ([`[rate_limit]`](../configuration/rate-limit.md)) is enabled.

> [!WARNING]
> Without `key_logic`, the bucket key is the caller identity each transport has: the client IP over HTTP, the topic for Kafka, the calling channel for `channel_call`. `requests_per_second: 100` therefore admits 100/s **per HTTP client**, plus 100/s from the channel's Kafka topic, plus 100/s per calling channel — a per-caller rate, not a throughput cap. For one shared bucket, give `key_logic` an expression that returns the same value on every ingress:

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 20,
    "key_logic": { "var": "channel" }
  }
}
```

**`key_logic` context.** The expression evaluates against exactly:

```json
{ "client_ip": "…", "channel": "…", "headers": { } }
```

`client_ip` is the transport's caller identity (it keeps that name on all four ingresses). `headers` contains these headers, when present: `authorization`, `x-api-key`, `x-forwarded-for`, `x-real-ip`, `user-agent`, `content-type`, `origin`, `x-tenant-id` — plus any name the channel lists in `key_headers`. No other header is visible to `key_logic`. A non-string result is serialized to its JSON text and used as the key.

**`key_headers`** is what makes a house header keyable — a `deviceId`, an `x-client-id`, an `x-partner`:

```json
{
  "rate_limit": {
    "requests_per_second": 5,
    "key_headers": ["deviceid"],
    "key_logic": { "var": "headers.deviceid" }
  }
}
```

Names are matched case-insensitively; they are lowercased at load. The list **adds to** the built-in set rather than replacing it. Declaring a header can never take `x-tenant-id` away from an expression that already reads it. Listing a built-in again is a no-op. The set stays closed by default because the request path materializes exactly the names that might be read. A channel does not pay an allocation per header for a key that references one of them.

> [!WARNING]
> A header is caller-supplied and therefore spoofable, so a key derived from one bounds an **honest** client. That is the right trade for a burst control, and the wrong one for a quota: forging a token-bucket key gets you a different bucket, not a bigger one, but forging a quota key is the whole attack. For per-user quotas, count in the workflow — `db_write`/`mongo_write` can increment and read back atomically, and keep this guard on top as the per-caller burst control.

The key is part of the control, not a hint:

- A `key_logic` that does not compile quarantines the channel at load.
- A request whose key cannot be evaluated is rejected with `429`. Nothing falls back to `client_ip` — that would silently re-dimension a per-tenant limit into a per-IP one.
- A request whose key evaluates to `null` or an empty string is rejected the same way. A missing path resolves to `null`, so a `key_logic` naming a header outside the set above would otherwise make the bucket key the literal string `"null"` for **every** caller — one shared bucket, and a limit that reads as enforced while enforcing nothing. Orion warns at channel load when an expression statically reads a header the context does not carry, so a typo surfaces at boot rather than as unexplained throttling.

**Cross-ingress semantics.** A Kafka record refused by the limit is not dead-lettered: its offset stays uncommitted and the consumer's capped retry backoff becomes the throttle. The exception is a `key_logic` that cannot be evaluated against the record. That fails identically on every redelivery, so the record is dead-lettered instead of blocking its partition.

**Cluster mode.** Per-channel limits enforce as a shared fixed window on the cluster Redis, so the configured rate holds across all replicas combined. Platform-level limits stay per node. See [Cluster Mode](../../operate/deploy/cluster.md).

Limiter state survives engine reloads: a channel whose `requests_per_second`, `burst`, `key_logic`, and `key_headers` are unchanged keeps its limiter, and consumed burst is not refilled. Editing any of them re-dimensions the buckets, so the limiter is rebuilt. Behind a proxy, set [`rate_limit.trusted_proxies`](../configuration/rate-limit.md) in the server config. Without it, every client behind the proxy keys on the proxy's address and collapses into one bucket.

## Related

- [Rate limit settings](../configuration/rate-limit.md): the platform limiter and `trusted_proxies`.
- [Configure a channel](../../guides/author/channels.md): adding a limit in practice.
- [`principal_rate_limit`](./principal_rate_limit.md): the quota keyed on the verified principal.
- [Deploy a cluster](../../operate/deploy/cluster.md): how the limit is shared across replicas.
- [Channel configuration](./index.md): every key, with its page.
