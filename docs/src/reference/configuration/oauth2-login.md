<!-- description: The [oauth2_login] settings: the instance-wide egress policy for token endpoints on private addresses, and what a channel's block owns instead. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-27 -->

# Inbound OAuth2 sign-in settings

Egress policy for a channel that completes a browser authorization-code grant, and — the one exception — the **identity providers a deployment supplies** for a channel that opts in. Everything else describing an identity-provider relationship belongs to the channel's [`oauth2_login` block](../channel-config/oauth2_login.md), because it is part of the definition and is promoted with it: the endpoints, the client credentials, the scopes, PKCE and the state cookie.

## Synopsis

```toml
[oauth2_login]
allow_private_token_urls = false

# A deployment-supplied provider, merged into any channel whose block sets
# providers_from_instance = true.
[oauth2_login.providers.iitm]
authorize_url = "https://login.microsoftonline.com/${IITM_TENANT}/oauth2/v2.0/authorize"
token_url     = "https://login.microsoftonline.com/${IITM_TENANT}/oauth2/v2.0/token"
client_id     = "${IITM_CLIENT_ID}"
client_secret = "env://IITM_CLIENT_SECRET"
scopes        = ["openid", "profile"]
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `oauth2_login.allow_private_token_urls` | `false` | `ORION_OAUTH2_LOGIN__ALLOW_PRIVATE_TOKEN_URLS` | Turn on for an identity provider on a private address, or a mock one in a test harness. |
| `oauth2_login.providers.<slug>` | — | file only | Add an identity provider a channel picks up with `providers_from_instance`. |

`token_url` is authored input that Orion sends the client secret and the authorization code to. It gets the same treatment `jwks_url` does: `https://` where it is authored, and the resolved address checked on every exchange. The setting is instance-wide for the same reason, too. A per-channel opt-out would let a definition grant itself the egress the setting gates.

`authorize_url` is not covered by this: Orion never fetches it, it only redirects the browser there. What guards that one is the `https://` requirement and the `return_to` allow-list.

## Deployment-supplied providers

*Which* identity providers exist differs per deployment — the public instance has one, a tenant's instance has its own directory as well — and that is a property of the deployment, not the promoted definition. `[vars]` holds literals, so it cannot add or remove a provider. `[oauth2_login.providers.<slug>]` can: a channel whose [`oauth2_login`](../channel-config/oauth2_login.md#several-providers-on-one-channel) block sets `providers_from_instance = true` merges these entries under its own `providers` map, and the definition's own entries win any slug clash. The definition then promotes between instances unchanged while each deployment supplies its own providers.

Each entry takes the per-provider fields — `kind`, `authorize_url`, `token_url`, `client_id`, `client_secret`, `client_auth`, `redirect_uri`, `scopes`, `extra_authorize_params`.

Two limits to know:

- **File only.** A nested, arbitrary-key map does not fit the `ORION_SECTION__KEY` environment scheme (the same limit `models.runtimes` and `plugins.overrides` have), so an individual provider cannot be set through an environment variable. Values inside still take `${VAR}` substitution — applied to the whole file before it is parsed, so `${IITM_TENANT}` works — and `env://NAME` references, resolved when the channel loads. That is how a per-deployment endpoint or secret is supplied.
- **Read at startup.** These are read once, at boot; they are not re-read on an engine reload. Adding or changing a provider here takes effect on a **process restart**. (A channel's own `providers` map, by contrast, is reload-scoped.) A provider whose shape is wrong quarantines every channel that merges it, named on `/health`.

## Related

- [Channel configuration › `oauth2_login`](../channel-config/oauth2_login.md): the block that describes one identity provider.
- [Secure an instance](../../operate/run/security.md): egress policy in context.
- [JWT verification settings](./jwt.md): the same policy for JWKS fetches.
- [Server configuration](./index.md): every section, by what you are configuring.
