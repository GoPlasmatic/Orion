<!-- description: The security settings of orion-server: admin authentication, JWKS and OAuth2 egress policy, CORS, platform rate limits and the channel filter. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Security settings

Admin authentication, JWT and OAuth2 egress policy, CORS, rate limits and the channel filter. Every table on these pages carries the wire name, the default the code uses, and the `ORION_*` override.

| Page | Holds |
|---|---|
| [Admin authentication settings](./admin-auth.md) | enabling admin authentication, the accepted and read-only API keys, hashed key storage, and the credential header. |
| [JWT verification settings](./jwt.md) | the instance-wide egress policy for JWKS fetches, and why the per-issuer settings live on the channel or the jwt_verify task instead. |
| [Inbound OAuth2 sign-in settings](./oauth2-login.md) | the instance-wide egress policy for token endpoints on private addresses, and what a channel's block owns instead. |
| [CORS settings](./cors.md) | allowed origins, the additive allowed and exposed header lists, credentials, and preflight caching, with the production rules. |
| [Rate limit settings](./rate-limit.md) | the platform token bucket, per-plane limits, and trusted_proxies, which decides whether forwarded headers name the client. |
| [Channel filter settings](./channel-filter.md) | include and exclude glob patterns over channel names, for running separate fleets off one database. |

## Related

- [Server configuration](./index.md): every section, by what you are configuring.
- [How settings are resolved](./how-settings-are-resolved.md): defaults, the file, and the environment.
- [Production checklist](../../operate/production-checklist.md): which settings to change before real traffic.
