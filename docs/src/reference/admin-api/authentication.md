<!-- description: The admin API key: the single header the server reads, the plain and sha256 key forms, rotation, and the per-source backoff after failures. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Authentication

The credential every admin endpoint requires when `admin_auth.enabled` is true, and what happens when one is wrong.

Admin API endpoints require an API key when `admin_auth.enabled` is true. The server reads the key from **exactly one header**: the one named by `admin_auth.header`, which defaults to `Authorization` (with or without a `Bearer ` prefix):

```bash
# Default configuration (admin_auth.header = "Authorization")
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

To use a custom header instead, set `admin_auth.header`, and note this
*replaces* the default, it does not add a second accepted header. With
`header = "X-API-Key"`, an `Authorization: Bearer` credential is no longer read:

```bash
# Requires admin_auth.header = "X-API-Key" in config
curl -H "X-API-Key: your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

Configure it through `[admin_auth]` in config, or the `ORION_ADMIN_AUTH__ENABLED=true` environment variable. Keys listed under `admin_auth.read_only_api_keys` authorise `GET`/`HEAD` only; every mutating method answers `403`.

## Failed-auth backoff

**Rationale.** This fixed policy limits credential guessing without adding
another security setting that can be disabled accidentally. The values below are part of server behavior; clients only need to handle the resulting `401` and retry conservatively.

Wrong credentials are rate-limited, so the admin plane cannot be guessed at line speed. The policy is fixed — there is no setting for it:

| Rule | Value |
|---|---|
| Tolerated before backoff starts | 5 consecutive failures from one client |
| First lockout | 500 ms, doubling on each further failure |
| Ceiling | 30 s, so a shared NAT egress address cannot be locked out indefinitely |
| Forgotten after | 300 s with no failure from that client |

A locked-out request answers the same `401 Invalid API key` a wrong key gets: the response never reveals that the caller is in backoff. One successful authentication clears the budget.

Two details are worth stating outright:

- **The client is identified by the `rate_limit.trusted_proxies` policy**, not
  by a raw `X-Forwarded-For`, so a forged header cannot mint a fresh budget per
  request. Behind an unlisted proxy every caller shares one budget. See the
  `client_ip` note under [Audit Logs](./audit-logs.md).
- **`GET /traces/{id}` shares the same budget.** It authenticates itself with a
  per-submission trace token rather than through the middleware, so a wrong
  token counts as a failure and a correct one clears the budget, exactly as an
  admin key does.

A read-only key refused on a mutation is a `403` and does **not** count: the credential is valid, only its authority is not. Each outcome increments `orion_admin_auth_failures_total` under its own `reason`. See the [Metrics Reference](../metrics.md).

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Admin authentication settings](../configuration/admin-auth.md): the `[admin_auth]` block behind this.
- [Secure an instance](../../operate/run/security.md): the admin plane as a control.
- [Errors and response envelopes](../errors.md): every code these endpoints return.
