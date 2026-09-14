<!-- description: The [jwt] settings: the instance-wide egress policy for JWKS fetches, and why the per-issuer settings live on the channel or the jwt_verify task instead. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# JWT verification settings

Instance-wide policy for JWT verification: the operator's egress rules for fetching keys. Per-issuer settings — algorithms, issuer, audience, leeway, the JWKS URL — belong to the channel's [`auth` block](../channel-config/auth.md) or the [`jwt_verify`](../functions/jwt_verify.md) task. Each of those describes one issuer relationship.

## Synopsis

```toml
[jwt]
allow_private_jwks_urls = false
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `jwt.allow_private_jwks_urls` | `false` | `ORION_JWT__ALLOW_PRIVATE_JWKS_URLS` | Turn on for an issuer on a private address (an in-cluster Keycloak, a sidecar). |

`jwks_url` is authored input: a field of a channel's `auth` block or of a `jwt_verify` task. It is the one egress path in the runtime with no operator-configured connector behind it. So it is checked twice. The URL must be `https://` where it is authored. The address it resolves to is checked on **every fetch**, exactly as an `http` connector without `allow_private_urls` is. The split is deliberate. An admin API that resolves DNS before accepting a channel is an admin API that hangs when the issuer is down. A host that was public when the channel was stored can be private by the time it is dialled.

Setting this to `true` disables the address check for every JWKS fetch on the instance. It is instance-wide rather than per channel. A per-channel opt-out would let the author of a definition grant themselves the egress the setting exists to gate.

## Related

- [Channel configuration › `auth`](../channel-config/auth.md): the per-issuer settings of the `jwt` mode.
- [`jwt_verify`](../functions/jwt_verify.md): verification inside a workflow.
- [Secure an instance](../../operate/run/security.md): egress policy in context.
- [Server configuration](./index.md): every section, by what you are configuring.
