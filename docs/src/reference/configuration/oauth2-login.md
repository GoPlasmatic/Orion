<!-- description: The [oauth2_login] settings: the instance-wide egress policy for token endpoints on private addresses, and what a channel's block owns instead. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Inbound OAuth2 sign-in settings

Egress policy for a channel that completes a browser authorization-code grant. Everything describing one identity-provider relationship belongs to the channel's [`oauth2_login` block](../channel-config/oauth2_login.md), because it is part of the definition and is promoted with it. That is the endpoints, the client credentials, the scopes, PKCE and the state cookie.

## Synopsis

```toml
[oauth2_login]
allow_private_token_urls = false
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `oauth2_login.allow_private_token_urls` | `false` | `ORION_OAUTH2_LOGIN__ALLOW_PRIVATE_TOKEN_URLS` | Turn on for an identity provider on a private address, or a mock one in a test harness. |

`token_url` is authored input that Orion sends the client secret and the authorization code to. It gets the same treatment `jwks_url` does: `https://` where it is authored, and the resolved address checked on every exchange. The setting is instance-wide for the same reason, too. A per-channel opt-out would let a definition grant itself the egress the setting gates.

`authorize_url` is not covered by this: Orion never fetches it, it only redirects the browser there. What guards that one is the `https://` requirement and the `return_to` allow-list.

## Related

- [Channel configuration › `oauth2_login`](../channel-config/oauth2_login.md): the block that describes one identity provider.
- [Secure an instance](../../operate/run/security.md): egress policy in context.
- [JWT verification settings](./jwt.md): the same policy for JWKS fetches.
- [Server configuration](./index.md): every section, by what you are configuring.
