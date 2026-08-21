# Secure an Instance

Orion's defaults serve a laptop: admin auth off, TLS off, CORS wide open, and a
data plane anyone who can reach the port can call. This page is what you change
before anything you do not control can reach it.

Setting `environment = "production"` makes exactly five things fatal at startup
rather than advisory: `admin_auth` disabled, an admin key too weak to be one, a
`[cors] allowed_origins = ["*"]` wildcard, `server.verbose_errors = true`, and
`cluster.enabled` together with `storage.auto_migrate`. Everything else on this
page — **including TLS and per-channel data-plane `auth`** — is never gated by
production mode and stays your responsibility.

## Authenticate the admin plane

`/api/v1/admin/**` and `/metrics` are unauthenticated until you turn this on.
Anyone who can reach the port can rewrite your workflows.

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
read_only_api_keys = []
# header = "Authorization"    # Bearer format (the default)
# header = "X-API-Key"        # raw key format
```

- **Store digests, not keys.** A `sha256:<digest>` entry authenticates the same
  key without the config, the environment, or a `validate-config` dump ever
  holding it.
- **List more than one key.** Any match authorises, so rotation needs no window
  in which requests are refused.
- **Give read-only consumers read-only keys.** A `read_only_api_keys` entry
  authorises `GET`/`HEAD` and answers `403` to anything mutating — enough for a
  dashboard, an auditor, or a CI check, and not enough to rewrite a workflow.

Guessing is already rate-limited: after five consecutive failures a client is
put in a doubling backoff up to 30 s, and reading a trace by its token shares
that budget. The policy is fixed and needs no configuration —
[Admin API › Failed-auth backoff](../reference/admin-api.md#failed-auth-backoff)
is the contract. Watch `orion_admin_auth_failures_total`; a sustained
`invalid_key` or `locked_out` rate is someone trying.

With `header = "Authorization"` the key travels as `Bearer <key>`; any other
header name takes the raw value.

```bash
curl -H "Authorization: Bearer your-secret-key" http://localhost:8080/api/v1/admin/workflows
curl -H "X-API-Key: your-secret-key"            http://localhost:8080/api/v1/admin/workflows
```

## Decide how the data plane authenticates

> [!WARNING]
> **`admin_auth` does not protect `/api/v1/data/**`.** A data channel is open to
> anything that can reach the port unless *that channel* declares an `auth`
> block.

Three modes are built in, all configured per channel:

- **`api_key`** — a key compared in constant time against the SHA-256 of each
  accepted key.
- **`hmac`** — a signature (SHA-1/256/512) over a templated signing string —
  the raw body by default, timestamped schemes via `message` or a `preset` —
  verified before parsing. This covers the webhook schemes of Stripe, GitHub,
  Shopify, Slack, Zoom, and Webex.
- **`jwt`** — bearer-token verification, detailed below.

All three take `env://` references, mask their credential fields in API reads, and
answer a uniform `401` that never reveals which part failed. The full contract —
fields, defaults, and why Kafka and `channel_call` are exempt — is
[Channel Configuration › Authentication](../reference/channel-config.md#authentication).

**JWT verification is built in; OIDC flows and mTLS termination are not.** The
`jwt` auth mode verifies bearer tokens at ingress (static keys or a JWKS) and
exposes the verified claims at `metadata.auth.claims.*` — identity reaches the
workflow without a header-forwarding proxy whose stripping rules Orion cannot
validate. What stays out of scope is the IdP half (discovery, PKCE, userinfo)
and client-certificate termination: for those, front Orion with a gateway or
service mesh, and let the `jwt` mode verify what it forwards.

## Terminate TLS

Orion can terminate TLS itself, or sit behind something that does. Pick one and
be deliberate about it — a plaintext listener reachable beyond the host is how
admin keys leak.

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/server.crt"
key_path  = "/etc/orion/tls/server.key"
```

- **Certificates load at startup.** A missing or unreadable file is a startup
  failure, not a fallback to plaintext.
- **`Strict-Transport-Security` is set only when TLS is on**, so a plaintext
  deployment does not advertise a guarantee it cannot keep.
- **Terminating at a load balancer is equally valid.** Leave `server.tls` off,
  and make sure the hop between the balancer and Orion is a trusted network.

## Trust the right proxies

If anything proxies traffic to Orion — a load balancer, an ingress controller, a
service mesh — set `rate_limit.trusted_proxies`, **whether or not you enable the
platform rate limiter**:

```toml
[rate_limit]
trusted_proxies = ["10.0.0.0/8", "fd00::/8"]
```

The rate limiter's client identity is the TCP peer address. Behind a proxy that
peer is always the proxy, so every caller collapses into one bucket and real
traffic starts getting `429`s. Forwarded headers are honoured only when the peer
is on this list, because a client can send any header it likes. On Kubernetes
this is your pod or node CIDR; behind a cloud load balancer it is the balancer's
subnet.

A malformed entry fails startup even with the limiter disabled. Run
`orion-server validate-config` before you deploy. The reasoning is in
[Design Notes › Why forwarded headers are ignored by default](../reference/design-notes.md#why-forwarded-headers-are-ignored-by-default).

## Keep credentials out of the database

Author every connector with a reference rather than a literal:

```json
{ "config": { "type": "http", "auth": { "type": "bearer", "token": "env://STRIPE_KEY" } } }
```

References resolve at load time from the server's environment, so the stored row
holds a variable name. `vault://<api-path>#<field>` reads HashiCorp Vault when
`VAULT_ADDR` and `VAULT_TOKEN` are present, re-read on each reload so a rotated
token applies without a restart. `aws-sm://`, `gcp-sm://`, and `azure-kv://` are
reserved: a reference using one without a live resolver is refused rather than
passed to the backend as a literal credential.

For defence below the API, encrypt the connector configs at rest:

```toml
[storage]
connector_encryption_key = "env://ORION_SECRET_CONNECTOR_KEY"
```

A database dump then carries an opaque envelope (AES-256-GCM) instead of
credentials.

Reads of a connector are masked by allowlist — only the structural vocabulary
each type defines comes back readable, and everything else is `"******"`, so a
credential under an unanticipated key fails closed rather than shipping in
clear. The normative rules are
[Connector Types › Secret masking](../reference/connectors.md#secret-masking).

### What a failed call may repeat back

An error from `http_call` names the endpoint it could not reach, because that
is the diagnostic. Two things bound what that costs you.

**URLs are redacted where they appear.** The userinfo password and any query
value whose name reads as a secret (`pwd`, `api_key`, `sig`, …) are masked
before the URL reaches an error message, a log line, an OTel span, a trace row
or the DLQ. The match is by parameter *name*, so it closes the conventional
spellings and not an unconventional one — `?pwd=` masks, `?pass=` does not.
Treat it as a backstop, not the control. The control is not putting the
credential in the URL: use `auth`, or `query_params`, whose values resolve
from references and never enter the URL string.

**Upstream error bodies are previewed, not copied.** A non-2xx response
contributes at most the first 512 bytes of its body to the error message,
marked `… (truncated)` when it is cut. Anything a failing API echoes back —
a token, an account record, a stack trace — is bounded rather than persisted
whole into `traces` and `trace_dlq`. This limit is separate from and much
smaller than `max_response_size`, which governs the body a *successful* call
may return to the workflow.

Both matter because these strings outlive the request. They are persisted to
the trace, and an async caller can read its own trace back with the
`trace_token` returned by the `202` — an admin credential is not the only key
to them. If a connector must carry a secret an error could name, keep it out
of the URL rather than relying on redaction to catch it.

## Bound what connectors can reach

Connectors are refused an endpoint that could not belong to their backend, and
refused a private address unless you say otherwise. Two layers:

| Layer | Runs at | Checks |
|---|---|---|
| Scheme allow-list | create / update | The endpoint's scheme suits its backend — a `db` connector cannot hold `http://169.254.169.254/…` |
| Private-address check | first connection | The resolved address is not RFC 1918, loopback, link-local, CGNAT, or the cloud metadata range |

```json
{ "config": { "type": "db", "connection_string": "env://ORDERS_DB_URL", "allow_private_urls": true } }
```

Most databases and caches *are* private, so most deployments set
`allow_private_urls: true` on them. That is the intended outcome: the flag makes
reaching an internal address a stated decision instead of the default, which
keeps the unstated case — a workflow-authored connector reaching
`169.254.169.254` — refused. Because the driver re-resolves the hostname when it
dials, this is a guard rather than a guarantee; pair it with network-level
egress policy where the difference matters.

Then bound what workflows may *do* through a connector with its operation gates,
which are enforced at the connection regardless of what any workflow asks:

```json
{ "operations": { "delete": false, "raw_write": false } }
```

See [Connector Types › Operation gates](../reference/connectors.md#operation-gates).

## Check origins server-side

Two different things share the word "origin", and conflating them is the common
mistake:

- **`origin_allow_list`** is a per-channel server-side check: a request whose
  `Origin` header is not listed is refused. It is enforcement.
- **`[cors]`** is the platform's browser handshake: it tells a browser what it
  may do. It is not enforcement — a non-browser client ignores it entirely.

Set the first when a channel should only serve named origins. Set the second so
browsers behave. Neither is authentication: `Origin` is client-supplied. The
per-channel check is specified in
[Channel Configuration › CORS & origins](../reference/channel-config.md#cors--origins);
the instance-level handshake is [Configuration › CORS](../reference/configuration.md#cors).

**Credentialed CORS widens what a browser will do on a user's behalf.**
`cors.allow_credentials = true` lets any page on a listed origin send the user's
cookies to Orion and read the response — so the origin list becomes a trust
boundary, not a convenience. Two consequences worth stating:

- Every origin you list is one that can act as a logged-in user. List the
  applications you operate, never a wildcard subdomain or a CDN you share.
- Orion refuses `allow_credentials` together with `allowed_origins = ["*"]` at
  startup. That is not a nicety: browsers reject the combination anyway, and the
  underlying layer asserts on it at router construction, so the alternative is a
  process that crashes at boot.

Credentialed cross-origin sessions usually also need `set-cookie` in
`cors.additional_exposed_headers` before a page script can see it.

## Close the surfaces you do not need

- **Swagger UI and the OpenAPI spec** publish the complete admin API to
  anonymous callers. `server.docs.enabled` unset serves them only outside
  production; set it `false` to be explicit. `orion-server dump-openapi` still
  writes the spec offline.
- **`/metrics` is admin-authenticated** along with the rest of the admin plane —
  and that credential can also rewrite workflows. Give the scraper its own
  listener instead: `metrics.bind_addr = "127.0.0.1:9090"` moves the endpoint to
  a separate unauthenticated port where the address *is* the access control.
- **Payload size** is capped by `ingest.max_payload_size` (1 MB by default).
  Raise it deliberately; it is the bound on what one request can cost you.

## Related

- [Production Checklist](./production-checklist.md) — every pre-go-live item,
  including the ones on this page, as one list.
- [Cluster Mode & High Availability](./cluster.md) — what changes about all of
  this once there is more than one replica.
- [Channel Configuration](../reference/channel-config.md) — the per-channel
  `auth`, `origin_allow_list`, and validation contracts.
- [Connector Types](../reference/connectors.md) — secret references, masking,
  and operation gates in full.
- [Configuration Reference](../reference/configuration.md) — every key named
  here, with its default and environment variable.
