<!-- description: The oauth2_login block: a channel as the relying party in a browser OAuth2 authorization-code grant, with PKCE, the state cookie, id_token checks and return_to. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

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
| `authorize_url` | string | yes | — | The provider's authorization endpoint. `https` only. Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `token_url` | string | yes | — | The provider's token endpoint. `https` only, and address-checked on every exchange unless [`oauth2_login.allow_private_token_urls`](../configuration/oauth2-login.md) is set. Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `client_id` | string | yes | — | The OAuth2 client identifier. Literal, `var://name` for a per-environment value, or `env://NAME`. |
| `client_secret` | string | yes | — | The client secret. `env://NAME` or `vault://…`; a literal works but puts the secret in the stored definition. |
| `client_auth` | string | no | `basic` | How credentials are presented at the token endpoint: `basic` (RFC 6749 §2.3.1) or `body`. |
| `redirect_uri` | string | yes | — | The absolute redirect URI registered with the provider, `https` only. Sent on both legs, because RFC 6749 §4.1.3 requires them to match. It differs on every environment, so `var://name` (or `env://NAME` / `vault://…`, resolved at load) is the usual spelling. |
| `callback_path` | string | yes | — | The callback route, as a second path on this channel. Static — no `{param}` segments — and must differ from `route_pattern`. |
| `scopes` | array of strings | no | `[]` | Requested scopes, space-joined. Empty sends no `scope` parameter. |
| `extra_authorize_params` | object | no | `{}` | Extra query parameters on the authorize URL (`prompt`, `hd`, `allow_signup`). Naming a reserved parameter is a create-time error; see [Reserved authorize parameters](#reserved-authorize-parameters). |
| `pkce` | boolean | no | `true` | PKCE (RFC 7636), S256 only. `plain` is not representable. |
| `state_secret` | string | yes | — | HS256 key for the state cookie. `env://NAME` or `vault://…`, at least 32 bytes. Must be identical on every node. |
| `state_cookie` | object | no | see [`state_cookie`](#state_cookie) | The state cookie's attributes. |
| `run_workflow_on_authorize` | boolean | no | `false` | Run the workflow on the authorize leg before the redirect is built. |
| `return_to` | object | no | — | `{param, allow_list}` — carry a pre-login destination through the flow. |
| `id_token` | object | no | — | OIDC `id_token` verification. Absent is plain OAuth2. |

### `state_cookie`

The fields are `name` (default `orion_oauth_state`), `secure` (default `true`), `same_site` (default `lax`), `path` (default `/`), and `max_age` in seconds (default `600`). `max_age` is also the state token's expiry: the window a user has to finish the consent screen. It must be between `1` and `86400` (24 hours). It sizes one consent screen, not a session, and a long one keeps a replayable state token valid for as long as it lasts.

### `id_token`

The fields are `issuer` (required, accepted `iss` values), `jwks_url` (required, `https`), and `audience` (defaults to `[client_id]`, per OIDC Core §3.1.3.7). The rest are `algorithms` (default `["RS256"]`), `required` (default `true`), and `nonce` (default `true`).

**Per-environment values.** Any value in the block may be `var://name`, substituted from the instance's `[vars]` when the channel loads. `env://NAME` and the vault schemes are resolved in `client_id`, `client_secret`, `state_secret`, `authorize_url`, `token_url` and `redirect_uri` only. A secret reference anywhere else is refused at create, because nothing would resolve it and its text would reach the provider. Create-time validation checks what it can see and defers a reference it cannot. The `https` rule and the rest of the shape are applied to the resolved value at load. A value that fails them quarantines the channel rather than serving it.

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
