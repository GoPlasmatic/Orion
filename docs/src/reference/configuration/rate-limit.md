<!-- description: The [rate_limit] settings: the platform token bucket, per-plane limits, and trusted_proxies, which decides whether forwarded headers name the client. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Rate limit settings

Platform-level limits, applied per client IP on each node. Per-channel limits are separate and live in the channel's `config_json` in the database.

## Synopsis

```toml
[rate_limit]
enabled = false
default_rps = 100
default_burst = 50
trusted_proxies = []

[rate_limit.endpoints]
admin_rps = 20
# data_rps = …   # no default
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `rate_limit.enabled` | `false` | `ORION_RATE_LIMIT__ENABLED` | Enable on any internet-facing instance. |
| `rate_limit.default_rps` | `100` | `ORION_RATE_LIMIT__DEFAULT_RPS` | Sustained requests per second per client. |
| `rate_limit.default_burst` | `50` | `ORION_RATE_LIMIT__DEFAULT_BURST` | Burst allowance above the sustained rate. |
| `rate_limit.trusted_proxies` | `[]` | `ORION_RATE_LIMIT__TRUSTED_PROXIES` | **Set this if Orion sits behind a load balancer**, whether or not `rate_limit.enabled` is on; see [`trusted_proxies`](#trusted_proxies). |
| `rate_limit.endpoints.admin_rps` | `20` | `ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS` | Separate limit for the admin API. Set the variable to an empty string to clear it, which makes the admin plane use `default_rps`. |
| `rate_limit.endpoints.data_rps` | — | `ORION_RATE_LIMIT__ENDPOINTS__DATA_RPS` | Separate limit for the data plane; unset means it uses `default_rps`. Set the variable to an empty string to clear it. |

### Limit scopes and combined enforcement

The platform default, admin, data, per-channel and per-principal controls remain
supported. They cover different traffic scopes:

| Control | Scope |
|---|---|
| `default_rps` | Per client IP on one node; operational routes and route groups without a separate limit share this limiter. |
| `endpoints.admin_rps` | Per client IP across admin routes on one node; unset uses the default limiter. |
| `endpoints.data_rps` | Per client IP across data routes on one node, including `server.data_mounts`; unset uses the default limiter. |
| Channel `rate_limit` | Optional per-channel buckets keyed by caller identity or `key_logic`; applies at the channel's ingresses. |
| Channel `principal_rate_limit` | Optional quota keyed by the verified principal after authentication. |

A data request must pass each applicable limit. For example, a caller can remain
within each channel's limit and still exhaust its platform data budget by calling
several channels. A channel without a `rate_limit` block still uses the platform
data limiter when `rate_limit.enabled = true`.

Platform limits stay local to each node. In cluster mode, channel limits use the
shared database; see [channel rate limits](../channel-config/rate_limit.md) and
[principal quotas](../channel-config/principal_rate_limit.md) for their semantics.

### `trusted_proxies`

The setting changes behaviour for every proxied deployment. The direct peer IP is authoritative. `X-Forwarded-For` and `X-Real-IP` are honoured *only* when the peer address falls inside one of these CIDR blocks. Bare IPs are accepted and treated as `/32` or `/128`. The default is empty, which means **forwarded headers are never trusted**. When the peer is trusted, the client is the **rightmost** `X-Forwarded-For` hop that is not itself a trusted proxy: the hop your own proxy appended. The leftmost elements arrive from the client verbatim and are never used, so a forged prefix cannot mint an identity.

The consequence in both directions:

- **Behind a load balancer with this unset**, every request appears to come from the balancer, so all clients share a single rate-limit bucket and the limit effectively applies to your whole fleet at once. List the balancer's subnet — `trusted_proxies = ["10.0.0.0/8"]` — to get per-client limiting back.
- **List a network you do not control** and clients on it can spoof `X-Forwarded-For` to mint a fresh bucket per request, which is exactly no rate limiting at all. List only the addresses of proxies you operate.

**It applies even with `rate_limit.enabled = false`.** The key is spelled under `[rate_limit]`, but what it configures is "may a forwarded header name the client". Four consumers resolve the caller's address with it: the platform rate limiter, the failed-admin-auth backoff, audit rows' `client_ip`, and per-channel `rate_limit` blocks. The full reasoning is in [Design Notes › Why forwarded headers are ignored by default](../../concepts/design-notes.md#why-forwarded-headers-are-ignored-by-default).

Both endpoint limits are optional, so their environment variables are three-state. Unset leaves the config-file value alone, a number sets the limit, and an empty string clears it back to "use `default_rps`".

## Related

- [Secure an instance](../../operate/run/security.md): trusted proxies in a deployment.
- [Channel configuration › `rate_limit`](../channel-config/rate_limit.md): the per-channel limiter.
- [Design notes › Why forwarded headers are ignored by default](../../concepts/design-notes.md#why-forwarded-headers-are-ignored-by-default): the reasoning behind `trusted_proxies`.
- [Server configuration](./index.md): every section, by what you are configuring.
