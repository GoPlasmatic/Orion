<!-- description: The [cors] settings: allowed origins, the additive allowed and exposed header lists, credentials, and preflight caching, with the production rules. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# CORS settings

The `[cors]` section: the browser handshake for cross-origin calls to any route on the instance.

## Synopsis

```toml
[cors]
allowed_origins = ["*"]
additional_allowed_headers = []
additional_exposed_headers = []
allow_credentials = false
# max_age_secs = …   # no default
```

## Description

CORS is instance-level. It cannot be configured per channel: preflights are answered before routing, so no channel is known yet. The per-channel [`origin_allow_list`](../channel-config/origin_allow_list.md) is a server-side origin *check*, not CORS.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `cors.allowed_origins` | `["*"]` | `ORION_CORS__ALLOWED_ORIGINS` | List explicit origins before production; comma-separated in the env var. |
| `cors.additional_allowed_headers` | `[]` | `ORION_CORS__ADDITIONAL_ALLOWED_HEADERS` | Your browser client sends a custom request header (`deviceid`, a tenant id, a trace header). |
| `cors.additional_exposed_headers` | `[]` | `ORION_CORS__ADDITIONAL_EXPOSED_HEADERS` | A page script needs to read a response header, for example `set-cookie` on login. |
| `cors.allow_credentials` | `false` | `ORION_CORS__ALLOW_CREDENTIALS` | Cookie-based cross-origin sessions. Requires explicit `allowed_origins`. |
| `cors.max_age_secs` | — | `ORION_CORS__MAX_AGE_SECS` | Cache preflights to cut their volume. Unset omits the header; capped at `86400`. |

Exactly `["*"]` is permissive CORS and is **rejected at startup** when `environment` starts with `prod`. Mixing `"*"` into a list of explicit origins is always a config error — it used to pass validation and then panic at router build.

**The two header lists are additive.** They extend the built-in sets; they cannot narrow them. Allowed: `content-type`, `authorization`, `accept`, `x-api-key`, `idempotency-key`, `x-request-id`. Exposed: `x-request-id`, `retry-after`. A replacing key would let an operator adding `deviceid` silently drop `authorization` and `content-type`. That would break admin auth, the dedup guard and every browser JSON call, with no server-side error anywhere. Header names are lowercased (`deviceId` → `deviceid`), which is correct: HTTP header names are case-insensitive and the preflight match is byte-lowercase. An entry that is not a valid header name is a startup error, not a silently dropped one.

**`allow_credentials` requires explicit origins.** The Fetch spec forbids pairing `Access-Control-Allow-Credentials: true` with `Access-Control-Allow-Origin: *`, and the browser rejects the response. The underlying layer asserts on the combination at router construction, so it would crash the process at boot. Orion refuses it during config validation instead. A `"*"` entry in either header list is refused for the same reason: it silently converts the explicit list back into a wildcard.

> [!NOTE]
> Orion sends the explicit allow-headers list on **every** configuration, including the wildcard-origin default. It previously sent `Access-Control-Allow-Headers: *` there, and per the Fetch Standard `Authorization` is a *CORS non-wildcard request-header name* that `*` never covers, so a browser calling the admin API with a bearer token failed preflight on a default install. Sending the list is strictly widening.

## Related

- [Channel configuration › `origin_allow_list`](../channel-config/origin_allow_list.md): the server-side origin check, which is not CORS.
- [Secure an instance](../../operate/run/security.md): browser clients and the admin plane.
- [Design notes](../../concepts/design-notes.md): why the header lists are additive.
- [Server configuration](./index.md): every section, by what you are configuring.
