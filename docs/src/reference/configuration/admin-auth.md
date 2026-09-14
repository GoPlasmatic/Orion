<!-- description: The [admin_auth] settings: enabling admin authentication, the accepted and read-only API keys, hashed key storage, and the credential header. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Admin authentication settings

Guards `/api/v1/admin/*` and the trace-read endpoints. Disabled by default so a fresh install is usable; **required** once `environment` starts with `prod` — startup fails without it.

## Synopsis

```toml
[admin_auth]
enabled = false
api_keys = []
read_only_api_keys = []
header = "Authorization"
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `admin_auth.enabled` | `false` | `ORION_ADMIN_AUTH__ENABLED` | Enable anywhere the admin API is reachable by anything but you. |
| `admin_auth.api_keys` | `[]` | `ORION_ADMIN_AUTH__API_KEYS` | Comma-separated in the env var. Any listed key authorises a request. |
| `admin_auth.read_only_api_keys` | `[]` | `ORION_ADMIN_AUTH__READ_ONLY_API_KEYS` | Keys limited to `GET`/`HEAD`; mutating methods answer `403`. For dashboards, auditors and CI checks. |
| `admin_auth.header` | `"Authorization"` | `ORION_ADMIN_AUTH__HEADER` | `"Authorization"` expects `Bearer <key>`; any other value (for example `"X-API-Key"`) expects the raw key. |

**Multiple keys exist for rotation.** Add the new key, roll clients over, then drop the old one — no restart gap where a valid client is refused.

**Keys may be stored hashed.** Each entry is either the plaintext key or `sha256:<64-hex>`, the SHA-256 digest of the key. The config file and any snapshot of it then hold a hash rather than a usable secret. Both forms verify the same presented token, and requests are compared at fixed width. Generate a digest with:

```bash
printf %s "$MY_ADMIN_KEY" | shasum -a 256
```

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
```

A malformed `sha256:` entry is a startup error, not a key that silently never matches. Audit entries record a hash prefix for hashed keys, so you can tell which key performed a mutation without storing the key.

## Related

- [Secure an instance](../../operate/run/security.md): admin auth in context, with key rotation.
- [Admin API › Authentication](../admin-api/authentication.md): how a key is presented on the wire.
- [Global flags](../cli/orion-cli/global-flags.md): how `orion-cli` supplies the key.
- [Server configuration](./index.md): every section, by what you are configuring.
