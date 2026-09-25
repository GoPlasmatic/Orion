<!-- description: The auth block of a channel: the api_key, hmac and jwt modes, every field each takes, the webhook presets, and the rules a failure and a rotation follow. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-16 -->

# `auth`

`auth` authenticates HTTP callers of a data channel. It covers `POST /api/v1/data/{channel}` and the `/async` submission identically — appending `/async` is not a bypass. Without an `auth` block, the channel is reachable by anyone who can reach the port; [`[admin_auth]`](../configuration/admin-auth.md) protects `/api/v1/admin` only.


## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `mode` | string | yes | — | `api_key`, `hmac`, or `jwt`. |
| `keys` | array of strings | `api_key` | — | Accepted keys; any match authorizes. Each entry is a literal or an `env://VAR` reference. |
| `header` | string | no | `Authorization` (`api_key`) / `X-Signature` (`hmac`) | Header carrying the credential. |
| `scheme` | string | no | `Bearer` when `header` is `Authorization`; none otherwise | `api_key` only: the [authentication scheme](#the-scheme) the header value must carry. An empty string means none — the bare key. |
| `secret` | string | `hmac` | — | Shared secret; literal or `env://VAR`. |
| `secrets` | array of strings | no | — | `hmac` only: additional accepted secrets, each tried in constant time — zero-downtime rotation. Merged with `secret`; at least one of the two is required. |
| `signature_prefix` | string | no | none | `hmac` only: prefix stripped from the signature before decoding, for example `sha256=`. Mutually exclusive with `signature_key`. |
| `signature_key` | string | no | none | `hmac` only: extract the signature from a comma-separated `k=v` packed header instead — Stripe's `v1`. Every occurrence is tried. |
| `algorithm` | string | no | `sha256` | `hmac` only: `sha1`, `sha256`, or `sha512`. The provider chooses; refusing `sha1` would only leave those webhooks unauthenticated. |
| `message` | string | no | `{body}` | `hmac` only: the signing-string template — literals plus `{body}` (required), `{header:<name>}`, and `{header:<name>:<key>}` for packed headers. Strictly parsed at create time. |
| `encoding` | string | no | auto-detect | `hmac` only: pins the presented signature encoding (`hex`, `base64`, `base64url`). Absent keeps auto-detection: hex first, then base64. |
| `timestamp` | string | no | — | `hmac` only: where the unix-seconds timestamp lives — `<header>` or `<header>:<key>`. Paired with `tolerance_secs`; either alone is a create-time error. |
| `tolerance_secs` | integer | no | — | `hmac` only: replay window in seconds around `timestamp`; requests outside it are refused before the MAC is computed. |
| `preset` | string | no | — | `hmac` only: `zoom`, `slack`, `stripe`, `github`, `shopify`, or `webex` — expands to the fields above; an explicitly set field overrides its preset row. |
| `jwt_keys` | array | no | — | `jwt` only: static verification keys `[{algorithm, key, kid?, key_encoding?}]`. At least one of `jwt_keys`/`jwks_url`. |
| `jwks_url` | string | no | — | `jwt` only: HTTPS JWKS URL — cached process-wide, single-flight refresh, stale-serve, `kid`-rotation refetch. |
| `algorithms` | array | `jwt` | — | `jwt` only: the mandatory non-empty allowlist (`HS/RS/PS 256-512`, `ES256/384`, `EdDSA`). Checked before anything else about a token — `alg: none` is unrepresentable. |
| `issuer` / `audience` | string \| array | no | — | `jwt` only: accepted `iss`/`aud` values. Absent skips the check. |
| `leeway_secs` | integer | no | `30` | `jwt` only: clock-skew allowance for `exp`/`nbf`, capped at 300. |
| `require_exp` | boolean | no | `true` | `jwt` only: tokens must carry `exp` (RFC 8725); opting out is deliberate config. |
| `required` | boolean | no | `true` | `jwt` only: `false` admits token-less requests with no `metadata.auth` key; a present-but-invalid token is still rejected. |
| `source` | object | no | `{"header": "Authorization", "scheme": "Bearer"}` | `jwt` only: `{"header": …, "scheme": …}` or `{"cookie": …}`. `scheme` is the [authentication scheme](#the-scheme) the header value must carry; omit it to read the whole header value as the token. Query parameters are deliberately not offered (RFC 6750 §2.3). |
| `max_token_bytes` | integer | no | `8192` | `jwt` only: token size cap. |
| `claims_to_metadata` | array | no | all claims | `jwt` only: which verified claims reach `metadata.auth.claims`. |
| `authorization_logic` | JSONLogic | no | — | `jwt` only: evaluated over `{"claims": …}` after verification; falsy → **403** `insufficient_scope`. An evaluation error fails closed. |

**`api_key`** compares the presented key in constant time against the SHA-256 of each accepted key. Listing several keys enables rotation without a window of refusals:

```json
{
  "auth": {
    "mode": "api_key",
    "keys": ["env://ORDERS_API_KEY", "env://ORDERS_API_KEY_PREVIOUS"],
    "header": "X-API-Key"
  }
}
```

**`hmac`** verifies an HMAC over a configurable signing string — by default the raw request body with SHA-256, exactly the pre-1.1 behavior. Verification runs on the bytes exactly as received, before any parsing, in constant time, against every listed secret.

Most providers are one preset:

```json
{ "auth": { "mode": "hmac", "preset": "zoom",   "secret": "env://ZOOM_WEBHOOK_SECRET" } }
{ "auth": { "mode": "hmac", "preset": "stripe", "secret": "env://STRIPE_WEBHOOK_SECRET" } }
{ "auth": { "mode": "hmac", "preset": "slack",  "secret": "env://SLACK_SIGNING_SECRET", "tolerance_secs": 60 } }
```

| Preset | Scheme it expands to |
|---|---|
| `zoom` | SHA-256 over `v0:{header:x-zm-request-timestamp}:{body}`, `v0=` hex in `x-zm-signature`, 300 s window |
| `slack` | SHA-256 over `v0:{header:x-slack-request-timestamp}:{body}`, `v0=` hex in `x-slack-signature`, 300 s window |
| `stripe` | SHA-256 over `{header:stripe-signature:t}.{body}`, signature from the packed header's `v1` keys, 300 s window |
| `github` | SHA-256 over `{body}`, `sha256=` hex in `x-hub-signature-256` |
| `shopify` | SHA-256 over `{body}`, base64 in `x-shopify-hmac-sha256` |
| `webex` | SHA-1 over `{body}`, hex in `x-spark-signature` |

An unlisted provider is the explicit form — configuration, never code:

```json
{
  "auth": {
    "mode": "hmac",
    "secret": "env://PARTNER_WEBHOOK_SECRET",
    "message": "v1:{header:x-request-timestamp}:{body}",
    "header": "x-partner-signature",
    "signature_prefix": "v1=",
    "timestamp": "x-request-timestamp",
    "tolerance_secs": 300
  }
}
```

One named non-goal: **Twilio**, whose base string needs the full public URL plus re-sorted form parameters — a per-provider algorithm, not a concatenation.

**`jwt`** verifies a bearer token at ingress and exposes the **verified claims**, never the token, at `metadata.auth.claims.*`. `validation_logic`, `authorization_logic`, the [response cache](./cache.md) `key_logic`, and every workflow task can read them there. The key is platform-reserved: a caller-supplied `metadata.auth` is stripped at ingress on every channel. Without a verified token, the key is absent rather than whatever the envelope said. That is the difference from fronting Orion with a gateway. A gateway can accept or reject, but it cannot give the workflow the identity (`sub`, roles) that per-user logic needs, except by forwarding spoofable headers.

```json
{
  "auth": {
    "mode": "jwt",
    "algorithms": ["HS512"],
    "jwt_keys": [{ "algorithm": "HS512", "key": "env://JWT_ACCESS_SECRET" }],
    "issuer": "example-api",
    "authorization_logic": { "in": ["teacher", { "var": "claims.roles" }] }
  }
}
```

Verification is fail-fast (RFC 8725): extract, then the allowlist (`alg: none` and downgrades die here), `kid` routing, the signature, then `exp`/`nbf`/`iss`/`aud` with leeway, then `authorization_logic`. A falsy `authorization_logic` answers 403; everything before it answers 401 with a `WWW-Authenticate: Bearer` challenge. A request with no bearer credential, meaning no token or another scheme, gets the bare `Bearer` challenge with no error code (RFC 6750 §3.1). A presented token that fails gets `Bearer error="invalid_token"`. Only **expiry** is described, as `error_description="token expired"`, because a client answers it with a refresh. The response body is the same for every cause, which is typed only in metrics (`orion_jwt_rejections_total{reason}`, where a wrong scheme is `scheme_mismatch`) and traces. Verified claims propagate through `channel_call`: one request, one identity. Static-key rotation is old + new entries under distinct `kid`s; issuer-side JWKS rotation is absorbed by the cache's refetch. Login and refresh flows are the [`jwt_sign` / `jwt_verify`](../functions/jwt_sign.md) task functions over the same core.

Rules:

- **A failure is always `401` with one message**, whatever the cause. The response never reveals whether the header was missing, the key wrong, the signature malformed, or the timestamp stale. A template header missing from the request refuses — never empty-string substitution, and the replay window is checked before any MAC work.
- **Auth configs are validated structurally at create/update/validate/import**: a missing `secret`, an unknown preset, a malformed template, or half a replay guard is a `400` naming the problem — not a channel quarantined at the next reload.
- **`env://` references resolve at channel load.** An `auth` block that cannot be built — an unset `env://` secret, for example — quarantines the channel rather than serving it unauthenticated.
- **`auth.keys`, `auth.secret`/`auth.secrets`, and `auth.jwt_keys[].key` are masked** as `"******"` in every API read. A masked value sent back on update is restored from the stored config; a sentinel with nothing to restore from is refused.
- **Kafka and `channel_call` are exempt by design.** A Kafka record carries no header and no signature; its authentication is the broker connection's (SASL/mTLS). A `channel_call` is a step inside a request that already authenticated at its own ingress and holds no credential to present.
- OIDC flows (discovery, PKCE, userinfo) and mTLS stay out of scope — the `jwt` mode verifies tokens; it is not an IdP. See [Secure an Instance](../../operate/run/security.md).

## The scheme

`scheme` (for `api_key`) and `source.scheme` (for `jwt`) name an HTTP authentication scheme. The header value is parsed as RFC 9110 §11.1 defines it: the scheme, one or more spaces, then the credential. The scheme matches case-insensitively, so with `"scheme": "Bearer"` all of these are accepted:

```text
Authorization: Bearer <credential>
Authorization: bearer <credential>
Authorization: BEARER <credential>
Authorization: Bearer  <credential>
```

`Authorization: Bearer<credential>`, with no space, is refused: the text before the first space is not the scheme. The credential after the spaces is compared byte-for-byte.

The value is a scheme name, not a prefix, so `"Bearer"` and `"Bearer "` are the same scheme. A value that is not an RFC 9110 token, such as `"Key="` or `"Bearer:"`, cannot be a scheme name. It is a `400` at create, update, validate and import, and an error from `lint` and `package lint`. A channel already stored with one is quarantined at load. `orion-server preflight` reports a stored one as `channel-auth`.

## Related

- [Secure an instance](../../operate/run/security.md): data-plane authentication in context.
- [Configure a channel](../../guides/author/channels.md): adding a guard in practice.
- [Environment variables](../environment-variables.md): the `env://` references the key fields take.
- [`jwt_verify`](../functions/jwt_verify.md): verifying a token inside a workflow instead.
- [Channel configuration](./index.md): every key, with its page.
