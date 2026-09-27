<!-- description: The oauth2_login block: a channel as the relying party in a browser OAuth2 authorization-code grant, with PKCE, the state cookie, id_token checks and return_to. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-27 -->

# `oauth2_login`

`oauth2_login` makes a channel the relying party in a browser OAuth2 authorization-code grant (RFC 6749 §4.1) — "Sign in with GitHub", "Continue with Google". Orion owns the redirect, the state cookie, the CSRF binding, PKCE, the code exchange and, for OIDC, `id_token` verification. The workflow keeps the application half and receives the grant at `metadata.oauth`.

## Synopsis

```json
{
  "channel_id": "github-signin",
  "protocol": "rest",
  "methods": ["GET"],
  "route_pattern": "/v1/auth/github",
  "workflow_id": "github-signin",
  "config": {
    "response": { "mode": "shaped", "cookies": true },
    "oauth2_login": {
      "authorize_url": "https://github.com/login/oauth/authorize",
      "token_url": "https://github.com/login/oauth/access_token",
      "client_id": "var://github_client_id",
      "client_secret": "env://GITHUB_CLIENT_SECRET",
      "redirect_uri": "var://github_redirect_uri",
      "callback_path": "/v1/auth/github/callback",
      "scopes": ["read:user"],
      "state_secret": "env://ORION_SECRET_OAUTH_STATE"
    }
  }
}
```

## Description

This is *establishment*, not verification, which is why it is a `config` block rather than a fourth [`auth.mode`](./auth.md). The two compose: `oauth2_login` mints a session, and `auth.mode = "jwt"` with `source: {"cookie": …}` guards every route the session then reaches.

**The channel serves two routes.** Its `route_pattern` is the authorize leg, where you send a user to begin, and `callback_path` is where the identity provider sends the browser back. Both are gated for collisions at activation, like any other route.

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `issuer` | string | no | — | An OIDC issuer. Set it and omit the endpoints below to have Orion discover them and verify the `id_token` — see [OIDC discovery](#oidc-discovery). `https`; literal, `var://name`, or `env://NAME` / `vault://…`. |
| `authorize_url` | string | cond. | — | The provider's authorization endpoint. `https` only. Required unless `issuer` is set (then discovered; an explicit value still wins). Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `token_url` | string | cond. | — | The provider's token endpoint. `https` only, and address-checked on every exchange unless [`oauth2_login.allow_private_token_urls`](../configuration/oauth2-login.md) is set. Required unless `issuer` is set (then discovered). Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `client_id` | string | yes | — | The OAuth2 client identifier. Literal, `var://name` for a per-environment value, or `env://NAME`. |
| `client_secret` | string | yes | — | The client secret. `env://NAME` or `vault://…`; a literal works but puts the secret in the stored definition. |
| `client_auth` | string | no | `basic` | How credentials are presented at the token endpoint: `basic` (RFC 6749 §2.3.1) or `body`. |
| `redirect_uri` | string | yes | — | The absolute redirect URI registered with the provider, `https` only. Sent on both legs, because RFC 6749 §4.1.3 requires them to match. It differs on every environment, so `var://name` (or `env://NAME` / `vault://…`, resolved at load) is the usual spelling. With [`providers`](#several-providers-on-one-channel) it is a template and must contain `{provider}`. |
| `callback_path` | string | yes | — | The callback route, as a second path on this channel. Must differ from `route_pattern`. Static in the single-provider form; with [`providers`](#several-providers-on-one-channel) it carries exactly one `{provider}` segment. |
| `kind` | string | no | derived | `oidc` or `oauth2` — the establishment protocol. Omitted, it is derived (`oidc` when `id_token` is set, else `oauth2`). |
| `providers` | object | no | — | Several identity providers on one channel, selected by `{provider}`. See [Several providers on one channel](#several-providers-on-one-channel). Mutually exclusive with the flat provider fields above. |
| `providers_from_instance` | boolean | no | `false` | Merge the deployment's [`[oauth2_login.providers]`](../configuration/oauth2-login.md#deployment-supplied-providers) under this block's `providers` map (the definition's own entries win a clash). Implies multi-provider mode. |
| `scopes` | array of strings | no | `[]` | Requested scopes, space-joined. Empty sends no `scope` parameter. |
| `extra_authorize_params` | object | no | `{}` | Extra query parameters on the authorize URL (`prompt`, `hd`, `allow_signup`). Naming a reserved parameter is a create-time error; see [Reserved authorize parameters](#reserved-authorize-parameters). |
| `pkce` | boolean | no | `true` | PKCE (RFC 7636), S256 only. `plain` is not representable. Shared by every provider. |
| `state_secret` | string | yes | — | HS256 key for the state cookie. `env://NAME` or `vault://…`, at least 32 bytes. Must be identical on every node. Shared by every provider. |
| `state_cookie` | object | no | see [`state_cookie`](#state_cookie) | The state cookie's attributes. Shared by every provider. |
| `run_workflow_on_authorize` | boolean | no | `false` | Run the workflow on the authorize leg before the redirect is built. Shared by every provider. |
| `return_to` | object | no | — | `{param, allow_list}` — carry a pre-login destination through the flow. Shared by every provider. |
| `id_token` | object | no | — | OIDC `id_token` verification. Absent is plain OAuth2. |

### `state_cookie`

The fields are `name` (default `orion_oauth_state`), `secure` (default `true`), `same_site` (default `lax`), `path` (default `/`), and `max_age` in seconds (default `600`). `max_age` is also the state token's expiry: the window a user has to finish the consent screen. It must be between `1` and `86400` (24 hours). It sizes one consent screen, not a session, and a long one keeps a replayable state token valid for as long as it lasts.

### `id_token`

The fields are `issuer` (required, accepted `iss` values), `jwks_url` (required, `https`), and `audience` (defaults to `[client_id]`, per OIDC Core §3.1.3.7). The rest are `algorithms` (default `["RS256"]`), `required` (default `true`), and `nonce` (default `true`).

### OIDC discovery

A provider that names an `issuer` and leaves `authorize_url`, `token_url` and the `id_token` block out is configured from `<issuer>/.well-known/openid-configuration` (OpenID Connect Discovery 1.0) at load:

```json
"oauth2_login": {
  "issuer": "https://login.microsoftonline.com/<tenant>/v2.0",
  "client_id": "var://entra_client_id",
  "client_secret": "env://ENTRA_CLIENT_SECRET",
  "redirect_uri": "https://app.example.com/v1/auth/entra/callback",
  "callback_path": "/v1/auth/entra/callback",
  "state_secret": "env://ORION_SECRET_OAUTH_STATE"
}
```

- Orion reads `authorization_endpoint`, `token_endpoint` and `jwks_uri` from the document, and **verifies the `id_token`** against the issuer and those keys — naming an `issuer` is what makes a provider OIDC, so there is no `id_token` block to hand-write (the defaults apply: `RS256`, `required`, `nonce`, audience `[client_id]`). An explicit endpoint or `id_token` block still wins.
- The document's own `issuer` must equal the configured one (OIDC §4.3), and every discovered endpoint must be `https` — a redirector at the well-known path cannot point Orion's key and token fetches elsewhere.
- It is fetched **at load, not per request**, on the shared SSRF-pinned client, cached with a TTL, refreshed in the background, and served stale through a transient issuer outage. The same [`allow_private_token_urls`](../configuration/oauth2-login.md) gate the token exchange uses applies. A cold fetch that fails quarantines the channel (named on `/health`); a warm cache carries a reload through a blip.

**Per-environment values.** Any value in the block may be `var://name`, substituted from the instance's `[vars]` when the channel loads. `env://NAME` and the vault schemes are resolved in `client_id`, `client_secret`, `state_secret`, `issuer`, `authorize_url`, `token_url` and `redirect_uri` only (including the per-provider copies of those fields). A secret reference anywhere else is refused at create, because nothing would resolve it and its text would reach the provider. Create-time validation checks what it can see and defers a reference it cannot. The `https` rule and the rest of the shape are applied to the resolved value at load. A value that fails them quarantines the channel rather than serving it.

### Several providers on one channel

One channel can serve many identity providers, chosen by a `{provider}` segment in its routes. Set `providers` — a map of slug to a per-provider block — *instead of* the flat provider fields, and put the `{provider}` segment in both `route_pattern` and `callback_path`:

```json
{
  "protocol": "rest",
  "methods": ["GET"],
  "route_pattern": "/v1/auth/{provider}",
  "config": {
    "oauth2_login": {
      "callback_path": "/v1/auth/{provider}/callback",
      "redirect_uri": "https://app.example.com/v1/auth/{provider}/callback",
      "state_secret": "env://ORION_SECRET_OAUTH_STATE",
      "providers": {
        "github": {
          "authorize_url": "https://github.com/login/oauth/authorize",
          "token_url": "https://github.com/login/oauth/access_token",
          "client_id": "var://github_client_id",
          "client_secret": "env://GITHUB_CLIENT_SECRET",
          "client_auth": "body",
          "scopes": ["read:user"]
        },
        "acme": {
          "authorize_url": "https://sso.acme.example/authorize",
          "token_url": "https://sso.acme.example/token",
          "client_id": "var://acme_client_id",
          "client_secret": "env://ACME_CLIENT_SECRET",
          "scopes": ["openid", "profile"],
          "id_token": { "issuer": ["https://sso.acme.example"], "jwks_url": "https://sso.acme.example/jwks" }
        }
      }
    }
  }
}
```

- **Each `providers` entry** carries the per-provider fields — `kind`, `authorize_url`, `token_url`, `client_id`, `client_secret`, `client_auth`, `scopes`, `extra_authorize_params`, `id_token`, and an optional `redirect_uri` override. The fields shared by every provider (`callback_path`, `redirect_uri`, `state_secret`, `state_cookie`, `pkce`, `run_workflow_on_authorize`, `return_to`) stay at the top.
- **The slug picks the entry on both legs.** `GET /v1/auth/github` sends the browser to GitHub; the callback at `/v1/auth/github/callback` exchanges the code with GitHub's credentials. An unknown slug is a `404`.
- **`redirect_uri` is a template.** `{provider}` is filled in with the slug, so each provider gets its own fixed URL to register (`…/v1/auth/github/callback`). A provider may override it with its own `redirect_uri`.
- **The slug is sealed into the signed state**, so a callback cannot present a state minted for one provider against another's callback URL — a mismatch is a `401`.
- **The workflow learns which provider answered** at `metadata.oauth.provider` (and its `metadata.oauth.kind`), so one workflow upserts on `(provider, subject)` and serves them all.

A block sets *either* `providers` *or* the flat provider fields, never both.

### A complete channel

`GET /api/v1/data/v1/auth/github` answers `302` to GitHub with a signed state cookie. GitHub redirects back to the callback, Orion verifies the state and exchanges the code, and the workflow runs with the grant in hand:

```json
[
  { "id": "identify", "function": { "name": "http_call", "input": {
      "connector": "github-api", "method": "GET", "path": "/user",
      "headers": { "authorization": { "cat": ["Bearer ", { "var": "metadata.oauth.access_token" }] } },
      "output": "temp_data.gh" } } },
  { "id": "upsert",  "function": { "name": "db_write",  "input": { "…": "upsert the user" } } },
  { "id": "session", "function": { "name": "jwt_sign",  "input": { "…": "mint the app's own token" } } },
  { "id": "respond", "function": { "name": "map", "input": { "mappings": [
      { "path": "data._orion.response", "logic": { "status": 302, "headers": { "location": "/" } } },
      { "path": "data._orion.response.cookies", "logic": [
        { "name": "session", "value": { "var": "temp_data.token" }, "path": "/",
          "http_only": true, "secure": true, "same_site": "Lax", "max_age": 2592000 } ] } ] } } }
]
```

### What the workflow receives

| Path | Present when |
|---|---|
| `metadata.oauth.access_token` | always |
| `metadata.oauth.token_type` | always (`Bearer`) |
| `metadata.oauth.expires_in` | the provider returned one |
| `metadata.oauth.scope` | the provider returned one |
| `metadata.oauth.refresh_token` | the provider returned one |
| `metadata.oauth.id_token` | the provider returned one |
| `metadata.oauth.claims` | `id_token` verification is configured and a token was verified |
| `metadata.oauth.return_to` | `return_to` is configured and the caller supplied a permitted value |
| `metadata.oauth.provider` | the channel is [multi-provider](#several-providers-on-one-channel) — the selected slug |
| `metadata.oauth.kind` | the channel is [multi-provider](#several-providers-on-one-channel) — `oidc` or `oauth2` |

`metadata.oauth` is platform-reserved: it is stripped from every caller-supplied envelope and written only by Orion, so a workflow reading it is reading a verified grant. It is also excluded from persisted task-detail snapshots, so the tokens in it are not written to disk.

### Reserved authorize parameters

`extra_authorize_params` may not set `client_id`, `redirect_uri`, `response_type`, `scope`, `state`, `nonce`, `code_challenge` or `code_challenge_method`. Naming one is a create-time `400`; a workflow contributing one under `run_workflow_on_authorize` has it ignored with a warning. Overriding `state` would disable the CSRF binding the block exists to provide.

### `run_workflow_on_authorize`

Off by default, the channel answers the redirect itself and the workflow is never entered. That is what makes the CSRF binding and the nonce unskippable.

Turn it on and the workflow runs first. It can refuse the sign-in by shaping its own `data._orion.response` (an unknown tenant, a maintenance window), or contribute to the redirect:

```json
{ "path": "data._orion.oauth2.authorize", "logic": {
    "extra_params": { "login_hint": { "var": "metadata.query.email" } },
    "scopes": ["read:user", "user:email"] } }
```

Orion still mints the state, the nonce and the PKCE challenge. The workflow cannot reach them and cannot replace them.

### `return_to`

```json
"return_to": { "param": "next", "allow_list": ["https://app.example.com/"] }
```

The value is read from that query parameter on the authorize leg and checked against the allow-list **there**. It is then sealed into the signed state, and handed back at `metadata.oauth.return_to`. Checking on the way in is what makes it safe to redirect to: a value that reaches the workflow has already passed. A value that has not is dropped silently. This is the one part of the flow a workflow cannot do for itself, because it never sees the authorize request.

An entry admits a candidate when the two have the **same origin**, with scheme, host and port all equal. The candidate's path must be the entry's path or lie beneath it at a `/` boundary:

| Allow-list entry | Admits | Refuses |
|---|---|---|
| `https://app.example.com` | anything on that host | `https://app.example.com.evil.test/steal` |
| `https://app.example.com/app` | `/app`, `/app/home` | `/application`, `/other` |

The match is on origin and path segments rather than on the text. The trailing slash is not load-bearing, and a host that *starts with* a permitted one is a different origin. Relative values (`/dashboard`) are not accepted; entries and candidates are both absolute URLs.

### What is refused, and why

- **`cache` alongside `oauth2_login`.** The response cache keys on the request and never on the caller, so a stored authorize `302` would replay one browser's state cookie to the next visitor and a stored callback would replay one user's session.
- **`same_site: "strict"` on the state cookie.** The callback is a top-level cross-site `GET` from the provider, so a `Strict` cookie is withheld on exactly that request and every sign-in fails the state check.
- **A non-`rest` protocol, or no `route_pattern`.** Both legs are routes; a channel reachable only by name has nowhere for the provider to send the browser back to.
- **`callback_path` equal to `route_pattern`.** They are two different requests and must be two different paths.
- **A `.../callback/async` submission.** `202` with a trace id is not a response a browser redirect can follow, and admitting it would run the workflow with no grant — a sign-in that appears to succeed and established nothing.

### Failures

Every callback refusal answers the same `401` with the same body. That covers a missing, expired, forged or mismatched state, a failed nonce check, a rejected `id_token`, a spent code, and a user who pressed Cancel. Naming the failing half would tell a prober which one to work on. The distinction lives in the log and in [`orion_oauth_login_total{outcome}`](../metrics.md), and the body is replaceable per channel with [`response.error_bodies`](./response.md#error-bodies). An unreachable identity provider answers `503` with `Retry-After`.

### Limits

Single use is enforced by clearing the state cookie on the callback, not by a stored row. Two concurrent replays of one callback inside the window would therefore both pass Orion's check. The authorization code itself is single-use at the provider, which is where that defence lives. Implicit and hybrid flows, the device-code grant, RP-initiated logout and end-user refresh-token rotation are all out of scope.

## Related

- [Secure an instance](../../operate/run/security.md): sign-in flows in context.
- [Inbound OAuth2 sign-in settings](../configuration/oauth2-login.md): the instance-wide egress policy.
- [`auth`](./auth.md): the `jwt` mode that guards the routes a session then reaches.
- [`response`](./response.md): shaping the redirect and setting the session cookie.
- [Channel configuration](./index.md): every key, with its page.
