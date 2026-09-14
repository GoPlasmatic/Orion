<!-- description: The jwt_verify task function: verify a JWS against static keys or a JWKS URL with an algorithm allowlist, issuer and audience checks, and leeway. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `jwt_verify`

Verifies a JWS mid-workflow (provider id_tokens for social login, refresh tokens, partner assertions) against static keys, a JWKS, or both. The JWKS cache is the process-wide one the channel's `jwt` mode uses: single-flight refresh, stale-serve, `kid`-rotation refetch. Rejections are typed task errors that `continue_on_error` branches on; the reason is named, the token never is.

## Synopsis

```json
{
  "name": "jwt_verify",
  "input": {
    "token": {
      "var": "data.id_token"
    },
    "algorithms": [
      "RS256"
    ],
    "keys": [],
    "jwks_url": "https://provider.example.com/certs",
    "issuer": "https://accounts.provider.example.com",
    "audience": "env://OAUTH_CLIENT_ID",
    "leeway_secs": 30,
    "require_exp": true,
    "output": "temp_data.verified_claims"
  }
}
```

## Description

`jwt_verify` is a utility function: self-contained, with no connector and no egress, so `dry-run` and `orion-server test` execute it for real.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `token` | string | yes | — | The compact JWS |
| `algorithms` | array | yes | — | Mandatory non-empty allowlist — `alg: none` and downgrades are unrepresentable |
| `keys` | array | one of | — | `[{algorithm, key, kid?, key_encoding?}]` — public halves for the asymmetric families. Each `key` takes `{"secret": "name"}`, a reference, or a literal |
| `jwks_url` | string | one of | — | HTTPS JWKS URL |
| `issuer` / `audience` | string \| array | no | — | Accepted `iss`/`aud` values; `{"secret": "name"}` and `env://` references resolve (OAuth client ids) |
| `leeway_secs` | number | no | `30` | Clock-skew allowance, capped at 300 |
| `require_exp` | boolean | no | `true` | RFC 8725: tokens must expire unless deliberately opted out |
| `output` | string \| JSONLogic | no | `"data"` | Where the verified claims object is stored |

## Examples

```json
{
  "name": "jwt_verify",
  "input": {
    "token": { "var": "data.id_token" },
    "algorithms": ["RS256"],
    "jwks_url": "https://provider.example.com/certs",
    "issuer": "https://accounts.provider.example.com",
    "audience": "env://OAUTH_CLIENT_ID",
    "output": "temp_data.verified_claims"
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Secure an instance](../../operate/run/security.md): where signing keys and secrets live in production.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Channel configuration › Authentication](../channel-config/auth.md): the `jwt` auth mode that verifies a token at ingress instead.
- [Environment variables](../environment-variables.md): the `env://`, `vault://` and `{"secret": …}` forms a key field takes.
