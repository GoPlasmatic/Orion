<!-- description: The auth block on http and es connectors: the bearer, basic and apikey schemes, and the managed oauth2 grant Orion acquires and refreshes itself. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Connector authentication

The `auth` object an `http` or `es` connector carries, and the one scheme Orion manages for you.

`http` and `es` connectors accept an `auth` object. Three schemes carry **static** credentials:

| Scheme | Fields | Example |
|--------|--------|---------|
| `bearer` | `token` | `{ "type": "bearer", "token": "env://API_TOKEN" }` |
| `basic` | `username`, `password` | `{ "type": "basic", "username": "svc", "password": "env://SVC_PASSWORD" }` |
| `apikey` | `header`, `key` | `{ "type": "apikey", "header": "X-API-Key", "key": "env://API_KEY" }` |

The fourth, [`oauth2`](#managed-oauth2), is **managed**: Orion acquires, caches, refreshes, and (under rotation) persists the token itself. `http` connectors only.

Put a token here rather than in an `Authorization` header: `"Bearer env://API_TOKEN"` in `headers` is sent as written, because a reference must be the whole value. See [A reference is the whole value](./secrets.md#a-reference-is-the-whole-value).

`db` and `cache` connectors carry credentials inside their connection URL instead. The `kafka` connector has no credential field; broker authentication is server configuration ([Kafka settings](../configuration/kafka.md)).

## Managed OAuth2

```json
"auth": {
  "type": "oauth2",
  "grant": "refresh_token",
  "token_url": "https://idp.example.com/oauth2/token",
  "client_id": "env://OAUTH_CLIENT_ID",
  "client_secret": "env://OAUTH_CLIENT_SECRET",
  "refresh_token": "env://OAUTH_REFRESH_TOKEN_SEED",
  "scopes": ["api.read"]
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `grant` | string | yes | — | `client_credentials` (server-to-server; tokens re-acquired on expiry), `refresh_token` (rotating service apps), or `account_credentials` (Zoom Server-to-Server OAuth) |
| `token_url` | string | yes | — | The IdP's token endpoint. Gets the same SSRF validation as the connector's own URL (`allow_private_urls` opts out) |
| `client_id` / `client_secret` | string | yes | — | The OAuth client. `client_secret` is masked on reads |
| `client_auth` | string | no | `basic` | How the client authenticates to the token endpoint (RFC 6749 §2.3.1): `basic` (HTTP Basic) or `body` (form parameters) |
| `refresh_token` | string | conditional | — | The bootstrap **seed** — required by the `refresh_token` grant, refused by the other grants. Masked on reads |
| `scopes` | array | no | — | Space-joined into the `scope` parameter |
| `audience` / `resource` | string | no | — | The corresponding token-request parameters (Auth0- / RFC 8707-style IdPs) |
| `account_id` | string | conditional | — | The Zoom account. **Required** by `account_credentials`, refused by every other grant. A tenant identifier rather than a credential, so it stays readable on admin reads and in package diffs |
| `extra_params` | object | no | — | Extra form parameters for provider quirks. Reserved names (`grant_type`, `client_id`, …) are refused; values are masked on reads (use `env://` refs if they must round-trip an export) |
| `refresh_margin_secs` | integer | no | `60` | Refresh this many seconds before expiry (max 3600) |

**Lifecycle.** Tokens are acquired lazily, cached in memory, and refreshed behind the margin. The refresh is single-flight, so concurrent requests wait on the in-flight one instead of racing it. Under rotation that race would invalidate the winner's new token. One acquisition serves every request in the cache window; a 401 from the API drops the cached token so the next call refetches. Token-endpoint outcomes land in the [`orion_oauth_token_requests_total`](../metrics.md) counter, and `POST /api/v1/admin/connectors/{id}/test` acquires a **real token**, validating the whole setup before any workflow depends on it.

**Rotation persistence.** A refresh response may carry a new refresh token. Orion persists it, with the access token and its expiry, to the `connector_oauth_state` table. The row is encrypted when `storage.connector_encryption_key` is set. The connector's own config is never mutated: it stays the declarative seed. The state row is stamped with a fingerprint of the `auth` block, so **editing the connector discards stale state**. That is also the recovery story for a burned token: update the connector with a fresh seed, and the seed wins. In cluster mode a refresh takes a job lease and other nodes adopt the persisted token instead of rotating against each other.

**Failures.** An unreachable token endpoint is retryable and trips the connector's circuit breaker like any outage. A rejection (`invalid_grant`, `invalid_client`) is a non-retryable error naming the OAuth error code. It is negative-cached for 30 s, so a burned token is never retry-looped against the IdP. It deliberately does not trip the API's breaker: a credential failure says nothing about the API's health.

**Zoom Server-to-Server OAuth** is the `account_credentials` grant — what Zoom moved every server-side integration to when it retired JWT apps in 2023. It exchanges `grant_type=account_credentials` plus the `account_id` with Basic client auth:

```json
{
  "type": "oauth2",
  "grant": "account_credentials",
  "token_url": "https://zoom.us/oauth/token",
  "client_id": "env://ZOOM_CLIENT_ID",
  "client_secret": "env://ZOOM_CLIENT_SECRET",
  "account_id": "env://ZOOM_ACCOUNT_ID"
}
```

Like `client_credentials` it re-acquires from static credentials, so it has no rotation state and takes no cluster lease. Caching, the refresh margin, single-flight, the failure split and the probe all apply unchanged. There is no workflow-level alternative: `http_call` headers are static, so a token a workflow fetched itself could never be attached to a request.

The `password` grant (ROPC) is deliberately absent — removed in OAuth 2.1. Future grants (`jwt-bearer`, token exchange, device code) are new `grant` values, not new auth types.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [`http`](./http.md): the connector type that accepts every scheme.
- [Secrets by reference](./secrets.md): holding the credential by reference rather than literally.
- [Request layering](./request-layering.md): where the auth header sits among the others.
