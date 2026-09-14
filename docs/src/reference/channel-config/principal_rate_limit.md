<!-- description: The principal_rate_limit block: a quota keyed on the verified JWT claims, applied after authentication on top of the address-keyed rate_limit. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `principal_rate_limit`

A per-caller quota keyed on **verified** identity, applied straight after authentication. `rate_limit` runs before authentication and can only key on the address or a caller-supplied header. That is a burst control, not a quota.

## Synopsis

```json
{
  "auth": { "mode": "jwt", "jwt_keys": [ ... ], "algorithms": ["RS256"] },
  "rate_limit": { "requests_per_second": 100 },
  "principal_rate_limit": {
    "requests_per_second": 10,
    "burst": 20,
    "key_logic": { "var": "auth.sub" }
  }
}
```

## Description

Both limits apply. `rate_limit` runs **before** authentication, deliberately, so a refusal costs the least work and credential-stuffing is metered like any other traffic. The consequence is that it cannot know who the caller is: the only identities it can key on are the address and a caller-supplied header. `principal_rate_limit` is the quota half, and the address limit stays the cheap outer guard.

The two limiters keep separate buckets and separate state across reloads. A refusal from either answers `429` and counts in `orion_rate_limit_rejections_total` under the channel's name, so 429 accounting stays whole. Which of the two refused is in the log line.

The block takes the same fields as `rate_limit`: `requests_per_second`, `burst`, `key_logic`, `key_headers`, `on_backend_error`. There are two differences, both refused at create rather than at run time:

- **`key_logic` is required.** The address limiter falls back to the caller
  identity when none is given; a principal has no such fallback, and inventing
  one would silently turn a per-user quota into a per-address one.
- **`auth.mode` must be `jwt`.** It is the only mode that exposes claims. On any
  other the key could never be computed and every request would be refused, so
  the config is refused instead.

Its `key_logic` context is the one `rate_limit.key_logic` reads plus `auth`, the verified claims. `{"var": "auth.sub"}` is the usual key, and `{"cat": [{"var": "auth.tenant"}, "|", {"var": "auth.sub"}]}` meters a tenant and a user together. Every rule above applies unchanged. A key that does not evaluate, or resolves to `null` or an empty string, is refused rather than bucketed somewhere wrong.

## Related

- [`rate_limit`](./rate_limit.md): the address-keyed limiter this runs after.
- [`auth`](./auth.md): the `jwt` mode that exposes the claims.
- [Secure an instance](../../operate/run/security.md): quotas versus burst controls.
- [Channel configuration](./index.md): every key, with its page.
