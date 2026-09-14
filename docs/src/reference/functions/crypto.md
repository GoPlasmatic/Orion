<!-- description: The crypto task function: digests, HMAC compute and verify, and argon2id or bcrypt password hashing as one self-contained operation envelope. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `crypto`

Digests, HMACs (compute **and** verify), and password hashing as one operation envelope. It is self-contained: no connector and no egress, so dry-run and `orion-server test` execute it for real.

## Synopsis

```json
{
  "name": "crypto",
  "input": {
    "op": "hmac",
    "algorithm": "sha256",
    "data": {
      "var": "data.payload.plainToken"
    },
    "input_encoding": "utf8",
    "key": "env://ZOOM_WEBHOOK_SECRET",
    "key_encoding": "utf8",
    "signature": "…",
    "password": "…",
    "hash": "…",
    "encoding": "hex",
    "params": {},
    "output": "temp_data.encrypted_token"
  }
}
```

## Description

`crypto` is a utility function: self-contained, with no connector and no egress, so `dry-run` and `orion-server test` execute it for real.

**Retry safety:** `pure`. See [Retry safety](./retry-safety.md) for what the answer costs.

The `op` field selects the operation, and each op takes the subset of fields below, checked when the workflow is created or validated. An op and algorithm pair outside the capability table, a missing `key`, or an out-of-bounds cost parameter is an authoring-time error.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `op` | string | yes | — | `hash` \| `hmac` \| `hmac_verify` \| `password_hash` \| `password_verify` |
| `algorithm` | string | no | per op | `hash`: `sha256` (default), `sha512`, plus `sha1`/`md5` for legacy interop. `hmac`/`hmac_verify`: `sha256` (default), `sha512`, `sha1`. `password_hash`: `argon2id` (default), `bcrypt`. `password_verify` auto-detects from the stored hash |
| `data` | any | for hash and HMAC ops | — | Bytes to digest. A string is UTF-8 (see `input_encoding`); any other JSON value is hashed as its compact serialization, key order preserved |
| `input_encoding` | string | no | `"utf8"` | How a *string* `data` becomes bytes: `utf8`, `hex`, `base64` |
| `key` | string | for HMAC ops | — | `{"secret": "name"}` reads the engine's [`[secrets]`](../configuration/vars-and-secrets.md) store; a string is a literal or a reference (`env://NAME`, `vault://…`). Never in traces or errors. Literals are fine for development; workflows are not encrypted at rest, so production wants one of the other two |
| `key_encoding` | string | no | `"utf8"` | How the resolved key becomes bytes: `utf8`, `hex`, `base64` — for APIs that issue binary signing keys |
| `signature` | string | for `hmac_verify` | — | The presented MAC; hex, base64, or base64url, auto-detected. Compared in constant time — never verify a MAC with `==` |
| `password` | string | for password ops | — | The submitted password |
| `hash` | string | for `password_verify` | — | The stored hash; scheme auto-detected from its `$argon2*$`/`$2*$` prefix, which is also the rehash-on-login discriminator |
| `encoding` | string | no | `"hex"` | Output encoding for `hash`/`hmac`: `hex`, `base64`, `base64url` (unpadded, the JWS form) |
| `params` | object | no | safe defaults | `password_hash` cost tuning, bounded: argon2id `memory_kib` (8192–131072, default 19456), `iterations` (1–10, default 2), `parallelism` (1–4, default 1); bcrypt `cost` (10–14, default 12) |
| `output` | string \| JSONLogic | no | `"data"` | Dotted result path. String for `hash`/`hmac`/`password_hash`; boolean for `hmac_verify`/`password_verify` |

A wrong password or a wrong signature answers `false`. A *malformed* stored hash or an undecodable signature is a task error, so data corruption is never mistaken for a bad credential.

## Examples

```json
{
  "name": "crypto",
  "input": {
    "op": "hmac",
    "algorithm": "sha256",
    "key": "env://ZOOM_WEBHOOK_SECRET",
    "data": { "var": "data.payload.plainToken" },
    "encoding": "hex",
    "output": "temp_data.encrypted_token"
  }
}
```

```json
{
  "name": "crypto",
  "input": {
    "op": "password_verify",
    "password": { "var": "data.password" },
    "hash": { "var": "temp_data.user.password_hash" },
    "output": "temp_data.password_ok"
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Secure an instance](../../operate/run/security.md): where signing keys and secrets live in production.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Environment variables](../environment-variables.md): the `env://`, `vault://` and `{"secret": …}` forms a key field takes.
