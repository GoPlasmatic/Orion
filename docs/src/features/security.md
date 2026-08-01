# Security

Orion enforces security at every layer: secrets are isolated in connectors, inputs are validated before processing, network requests are checked for SSRF, and admin endpoints are protected by authentication.

## Secret Management

Connector configs are masked **by allowlist** in every API response: only the
structural vocabulary the connector types define — endpoints, timeouts,
operation gates, identities like `username` — is served readable, and *every
other value* comes back as `"******"`. A credential under a key no list of
secret names anticipated (`signing_cert_pem`, a custom header value) therefore
fails closed instead of shipping in clear. Readable URL values still have
their in-band secrets redacted (`redis://user:******@host`,
`?api_key=******`), and `env://` secret *references* pass through unmasked —
they name a variable, not a value, and must survive `export` → `import`.

Secret **references** resolve at load time, so stored configs never hold the
value: `env://NAME` reads the process environment, and `vault://<api-path>#<field>`
reads HashiCorp Vault over its HTTP API when the standard `VAULT_ADDR` +
`VAULT_TOKEN` environment is present — KV v2 (`vault://secret/data/db#password`)
and KV v1 shapes both resolve, per engine reload, so a renewed token or rotated
secret is picked up by the next reload without a restart. The remaining
reserved schemes (`aws-sm://`, `gcp-sm://`, `azure-kv://`) fail closed: a
reference using one is refused at load rather than handed to a backend as the
literal credential.

For defence in depth below the API, `storage.connector_encryption_key`
encrypts `connectors.config_json` at rest (AES-256-GCM) — a database dump or
backup then carries an opaque envelope rather than credentials. See the
[Config Reference](../configuration/reference.md).

Channel configs mask their two credential fields exactly: `auth.keys` and
`auth.secret`. Both surfaces support the same GET → edit → PUT round-trip —
a value still reading `"******"` on update is restored from the stored config,
and a sentinel with nothing to restore from is refused rather than persisted
as the credential.

```bash
# Create a connector with real credentials
curl -s -X POST http://localhost:8080/api/v1/admin/connectors \
  -H "Content-Type: application/json" \
  -d '{
    "name": "payments-api",
    "connector_type": "http",
    "config": {
      "type": "http",
      "url": "https://api.stripe.com/v1",
      "auth": { "type": "bearer", "token": "sk-live-secret-token" }
    }
  }'

# Read it back (secrets are masked)
curl -s http://localhost:8080/api/v1/admin/connectors/<id>
# auth.token → "******"
```

Workflows reference connectors by name (`"connector": "payments-api"`). They never see or embed actual credentials. This means AI-generated workflows can be safely created and shared without risk of credential exposure.

## Input Validation

Each channel can define JSONLogic validation rules evaluated against incoming requests before workflow execution:

```json
{
  "validation_logic": {
    "and": [
      { "!!": [{ "var": "data.order_id" }] },
      { ">": [{ "var": "data.amount" }, 0] }
    ]
  }
}
```

If validation fails, the request is rejected with `400 Bad Request` before any workflow logic runs.

Validation rules have access to:

- `data.*`: request body fields
- `headers.*`: HTTP headers
- `query.*`: query string parameters
- `path.*`: path parameters (for REST channels)

**Payload size limits** are enforced globally to prevent oversized requests:

```toml
[ingest]
max_payload_size = 1048576   # 1 MB
```

## Network Security

**SSRF protection:** connectors validate their endpoints to prevent Server-Side
Request Forgery. By default, connections to private/internal IP addresses
(RFC 1918, loopback, link-local, CGNAT, and the cloud metadata range
`169.254.169.254`) are blocked:

```json
{
  "name": "external-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.example.com",
    "allow_private_urls": false
  }
}
```

Set `allow_private_urls: true` only when the target is intentionally on a
private network.

This applies to **every** connector type, not just `http`. Two layers enforce
it:

| Layer | When | What it checks |
|---|---|---|
| Scheme allow-list | create / update | The endpoint's scheme suits its backend |
| Private-address check | first connection | The resolved address is not private |

The scheme allow-list refuses a connector whose endpoint could not belong to
its backend — a `db` connector holding `http://169.254.169.254/…`, say:

| Type | Allowed schemes |
|---|---|
| `http` | `http`, `https` |
| `es` | `http`, `https` |
| `db` | `postgres`, `postgresql`, `mysql`, `mariadb`, `sqlite`, `mongodb`, `mongodb+srv` |
| `cache` (redis) | `redis`, `rediss` |
| `kafka` | *n/a* — brokers are bare `host:port`, validated for shape |

It runs on schemes only, never DNS: storing a connector must not depend on the
target being reachable.

The private-address check runs when the connection is first opened, and is
skipped where there is no address to judge — `sqlite:` opens a file, and
`backend: "memory"` opens nothing. For MongoDB the check runs against the
hosts the driver resolved, so a replica-set URI is checked host by host and a
`mongodb+srv://` URI is checked after its SRV record is looked up.

> **Databases and caches are usually private, so most deployments will set
> `allow_private_urls: true` on them.** That is the intended outcome: the flag
> exists so reaching an internal address is a stated decision rather than the
> default. Because the driver re-resolves the hostname when it dials, this is
> a guard rather than a guarantee — pair it with network-level egress policy
> where the distinction matters.

**TLS/HTTPS:** enable TLS termination in the server:

```toml
[server.tls]
enabled = true
cert_path = "cert.pem"
key_path = "key.pem"
```

**Security headers:** set on all responses:

| Header | Value |
|--------|-------|
| `X-Content-Type-Options` | `nosniff` |
| `X-Frame-Options` | `DENY` |
| `Content-Security-Policy` | `default-src 'none'; frame-ancestors 'none'` |
| `Referrer-Policy` | `strict-origin-when-cross-origin` |
| `Permissions-Policy` | `camera=(), microphone=(), geolocation=()` |
| `Strict-Transport-Security` | Set when TLS is enabled |

## Access Control

**Admin API authentication:** protect admin endpoints with bearer token or API key:

```toml
[admin_auth]
enabled = true
api_keys = ["your-secret-key"]   # Any number of accepted keys; any match authorises a request
read_only_api_keys = []          # Keys limited to GET/HEAD; mutating methods answer 403
# header = "Authorization"       # Bearer format (default)
# header = "X-API-Key"           # Raw key format
```

When `header` is `"Authorization"`, the key is expected as `Bearer <key>`. For any other header name, the raw key value is matched directly.

**Read-only keys** carry the same forms (plaintext or `sha256:` digest) but authorise `GET`/`HEAD` only — a dashboard, an auditor, or a CI check can list workflows and read traces without holding a credential able to rewrite them. A mutating request with a read-only key answers `403` (the credential is valid; its authority is not), and the refusal is counted under the `admin_auth_failures_total{reason="read_only_write"}` metric.

```bash
# Bearer token
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows

# API key via custom header
curl -H "X-API-Key: your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

**Per-channel authentication:** authenticate callers of a data channel in its `config_json`. `admin_auth` above covers `/api/v1/admin` only — without a channel `auth` block, a channel is reachable by anyone who can reach the port.

Two modes ship. Both are enforced in the ingress guards, so `POST /api/v1/data/{channel}` and `POST /api/v1/data/{channel}/async` are covered identically — an `/async` submission is not a way around the check.

**`api_key`** — a shared secret in a header, compared in constant time against the SHA-256 of each accepted key:

```json
{
  "auth": {
    "mode": "api_key",
    "keys": ["env://ORDERS_API_KEY", "env://ORDERS_API_KEY_PREVIOUS"],
    "header": "X-API-Key"
  }
}
```

Listing several keys is what makes rotation possible without a window of refusals: any match authorises. `header` defaults to `Authorization`, in which case the value is expected as `Bearer <key>`; any other header name takes the bare key. Override with `scheme` if you need a different prefix.

**`hmac`** — HMAC-SHA256 over the **raw request body**, which is how Stripe, GitHub and Shopify authenticate webhooks:

```json
{
  "auth": {
    "mode": "hmac",
    "secret": "env://GITHUB_WEBHOOK_SECRET",
    "header": "X-Hub-Signature-256",
    "signature_prefix": "sha256="
  }
}
```

The signature is verified against the bytes exactly as received, before any parsing — re-serializing parsed JSON reorders keys and drops whitespace, and the signature would never match again. Hex (Stripe, GitHub) and base64 (Shopify) encodings are both accepted. `signature_prefix` is stripped before decoding; omit it when the header carries a bare signature.

Any `keys` entry or `secret` may be an `env://VAR` reference, resolved at channel load by the same resolver connector secrets use, so production credentials never sit in the stored config.

Two properties worth stating explicitly:

- **A failure is always `401` with the same message**, whatever the cause. Distinguishing "no header" from "wrong key" from "malformed signature" would tell an unauthenticated caller which half of the credential they had right.
- **A channel whose `auth` cannot be built is quarantined**, not served unauthenticated. If an `env://` secret is unset on a host, that channel is refused at every ingress there rather than loaded with its authentication silently absent — the same posture a `validation_logic` that no longer compiles gets.

Authentication does **not** apply to the Kafka ingress or to `channel_call`, and the omission is deliberate. A Kafka record carries no HTTP header and no signature over a body the producer never signed; its authentication is the broker connection's (SASL/mTLS). A `channel_call` is a step inside a request that already authenticated at its own ingress, and the calling workflow holds no credential to present — enforcing there would make composition impossible rather than make it safer. A channel reachable both over HTTP and from a topic is therefore authenticated on the HTTP path and broker-authenticated on the Kafka one.

There is no built-in JWT verification yet. For OIDC/JWT, or for mTLS, put a gateway or service mesh in front.

**Per-channel origin allow-list:** restrict which `Origin` values a channel accepts, in its `config_json`:

```json
{
  "origin_allow_list": ["https://app.example.com", "https://admin.example.com"]
}
```

A request whose `Origin` header is present and unlisted is refused `403`. `"*"` allows any origin; omitting the key checks nothing.

**This is not CORS**, and it was called `cors.allowed_origins` until 1.0, which is why it needed renaming. It performs no handshake: it sets no `Access-Control-Allow-Origin` and takes no part in a preflight. The browser handshake is the platform CORS layer's job, configured in the `[cors]` section:

```toml
[cors]
allowed_origins = ["*"]    # Browser CORS policy, all routes
```

The two are complementary, and it is worth being exact about which one enforces what:

- **`[cors] allowed_origins` governs the browser handshake.** A genuine *preflight* (`OPTIONS` carrying `Access-Control-Request-Method`) from an unlisted origin is answered by the layer and never reaches a channel. But a non-preflighted cross-origin request — a simple `GET`, or a `POST` a browser sends without asking first — is *not* short-circuited: the layer simply omits `Access-Control-Allow-Origin`, the workflow runs server-side, and only the browser discards the response. And a non-browser client (curl, a server-to-server caller, anything setting `Origin` by hand) is unaffected by `[cors]` altogether.
- **`origin_allow_list` is the server-side check.** It runs in the ingress guards on every request that reaches the handler, browser or not, and refuses `403` before the workflow executes.

So if the point is to keep a workflow from *running* for an unlisted origin, `origin_allow_list` is the control that does it; `[cors]` alone is a browser-side courtesy. Note that neither is authentication: `Origin` is a client-supplied header and any non-browser caller can set it to anything, or omit it — a request with no `Origin` is not checked at all. For access control that holds against a hostile client, use the channel `auth` block above.

The pre-1.0 spelling is refused, not ignored:

```json
{ "cors": { "allowed_origins": ["https://app.example.com"] } }
```

A stored channel still carrying it fails to parse and is quarantined at load — refused at every ingress rather than served. Accepting the key and dropping it would leave the channel with no allow-list at all, indistinguishable from one that deliberately checks nothing, so every unlisted origin would be admitted silently. `orion-server preflight` names every stored channel still using it.

## Data Safety

**Parameterized SQL queries:** the `db_read` and `db_write` functions use parameterized queries to prevent SQL injection:

```json
{
  "function": {
    "name": "db_read",
    "input": {
      "connector": "orders-db",
      "query": "SELECT * FROM orders WHERE customer_id = $1",
      "params": [{ "var": "data.customer_id" }],
      "output": "data.orders"
    }
  }
}
```

Values are always passed as parameters, never interpolated into SQL strings.

**The portable dialect is injection-safe by construction:** `data_query` and
`data_write` never accept SQL or query text at all — message data enters only
through the `params` map, and every resolved value becomes a bound parameter
(SQL), a document value (MongoDB), or a script parameter (Elasticsearch
painless scripts, where field names *and* values travel as params). Identifiers
come only from the envelope and schema and are quoted per dialect.

**The dialect is bounded by default:** since 1.0 an undeclared entity or column
is rejected (`"unmapped": "reject"`), so a task reaches only what its inline
`schema` declares, with per-column `queryable`/`writable` flags. Through 0.x
the default was identity mode, which let any workflow author reach every table
the connector's database user could see. A connector can tighten this further
with `dialect.require_schema` (refuse the per-task `"unmapped": "identity"`
opt-out) and `dialect.allowed_entities` (a physical table allowlist that
renames cannot escape). Both bound the *portable dialect*: `db_read`,
`db_write` and `mongo_read` name no entity and are gated only by `operations`,
so a connector reachable by raw SQL is bounded by its database credential
rather than by its allowlist. See the
[Portable Data Dialect](../reference/data-dialect.md) reference.

**Write-safety guards:** an unfiltered `data_write` update/delete is rejected
unless the call carries `"all": true` **and** the server enables
`write.allow_unfiltered` — a double opt-in against accidental table truncation.
Bulk inserts over `write.max_rows` are rejected, never silently truncated.

**Per-connector operation gates:** every connector type's config can disable
operation types outright, all defaulting to allowed. A gated call fails with a
validation error naming the op and connector, so a connector can be made
read-only (or insert-only, delete-proof, …) in configuration, regardless of
what any workflow asks for:

| Type | Gates |
|------|-------|
| `db`, `es` | `read`, `insert`, `update`, `delete`, `upsert`, `raw_write` |
| `cache` | `read`, `write` |
| `kafka` | `publish` |
| `http` | `methods` — an allow-list; empty allows every method |

```json
{
  "name": "orders-db-readonly",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://…",
    "operations": { "insert": false, "update": false, "delete": false, "upsert": false, "raw_write": false }
  }
}
```

```json
{
  "name": "partner-api-readonly",
  "connector_type": "http",
  "config": { "type": "http", "url": "https://partner.example.com/v1", "operations": { "methods": ["GET"] } }
}
```

A gate key the type does not have — `{"writes": false}` on a cache — is a 400,
not a 201 with an open gate; and `cache`'s `write` covers every write through
the connector, including a channel dedup store or response cache backed by it.

**URL validation:** connector URLs are validated at creation time. Combined with SSRF protection, this prevents workflows from making requests to unexpected destinations.

**Injection protection:** JSONLogic expressions are evaluated in a sandboxed environment. User-supplied data cannot escape the data context or execute arbitrary code.
