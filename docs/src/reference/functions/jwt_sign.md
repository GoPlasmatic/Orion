<!-- description: The jwt_sign task function: mint a compact JWS with the HS, RS, PS, ES or EdDSA algorithms, a required expiry, and claims folded from the message. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `jwt_sign`

Mints a compact JWS — login access/refresh pairs, RFC 7523 client assertions.
Self-contained like `crypto`: no connector, real execution in dry-run, and the
signing key (a literal or an `env://`/`vault://` reference) lives only inside
the call. `iat` is stamped automatically **unless the claims object supplies
one**; a token must expire deliberately — `expires_in`, or an explicit `exp`
claim.

## Synopsis

```json
{
  "name": "jwt_sign",
  "input": {
    "algorithm": "…",
    "key": "…",
    "key_encoding": "utf8",
    "claims": {},
    "expires_in": 0,
    "issuer": "…",
    "audience": "…",
    "not_before": "…",
    "kid": "…",
    "output": "data"
  }
}
```

## Description

`jwt_sign` is a utility function: self-contained, with no connector and no egress, so `dry-run` and `orion-server test` execute it for real.

**Retry safety:** `pure`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `algorithm` | string | yes | — | `HS256/384/512`, `RS256/384/512`, `PS256/384/512`, `ES256/384`, `EdDSA` |
| `key` | string | yes | — | HS secret or RS/ES/Ed **private**-key PEM; `{"secret": "name"}`, a reference, or a literal |
| `key_encoding` | string | no | `"utf8"` | How an HS secret becomes bytes: `utf8`, `base64`, `hex` |
| `claims` | object | no | `{}` | Claim values fold `{"var": …}` nodes and nothing else — compose a computed claim in a `map` task first. (`audience`, `not_before` and `expires_in` below are full JSONLogic) |
| `expires_in` | number \| string | conditional | — | Lifetime (seconds or `"<n>s\|m\|h\|d"`) → `exp`. Required unless `claims.exp` is explicit |
| `claims.iat` | number | no | now | Issue time. Supplying one wins — there is no `issued_at` field, so nothing more specific can beat it. Back- or forward-dating is what revocation-pivot schemes need, and it is the only way a minted token can be asserted byte-for-byte offline |
| `issuer` / `audience` / `not_before` | — | no | — | Conveniences for `iss` / `aud` / `nbf` (offset from now); explicit fields win over same-named claims entries |
| `kid` | string | no | — | Key id stamped into the header, for rotation-aware verifiers |
| `output` | string \| JSONLogic | no | `"data"` | Where the token (string) is stored |

`iat` and `exp` supplied through `claims` must be **numbers**: seconds since
the Unix epoch (NumericDate, RFC 7519 §2). A string date is refused at sign
time rather than minting a token every verifier rejects later. Nothing in Orion
makes a trust decision on `iat`: neither `jwt_verify` nor the channel `jwt`
mode inspects it, so a back-dated token verifies normally.

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Secure an instance](../../operate/run/security.md): where signing keys and secrets live in production.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Environment variables](../environment-variables.md): the `env://`, `vault://` and `{"secret": …}` forms a key field takes.
