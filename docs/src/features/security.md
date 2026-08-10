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
The normative masking rules live in
[Connector Types › Secret masking](../reference/connectors.md#secret-masking).

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
[Config Reference](../reference/configuration.md).

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

Each channel can define a JSONLogic `validation_logic` rule, evaluated before
workflow execution; a failing request is rejected with `400` before any
workflow logic runs. The rule's evaluation context and configuration are
specified in
[Channel Configuration › Validation](../reference/channel-config.md#validation).

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

**Per-channel authentication:** callers of a data channel authenticate in its
`config_json` `auth` block — `api_key` mode or `hmac` webhook verification,
with `env://` secrets and uniform-`401` behavior. The full contract, including
the Kafka and `channel_call` exemptions, is in
[Channel Configuration › Authentication](../reference/channel-config.md#authentication).
There is no built-in JWT verification; for OIDC/JWT or mTLS, front Orion with
a gateway or service mesh.

**Per-channel origin allow-list:** `origin_allow_list` is the server-side
origin check; the browser CORS handshake belongs to the platform `[cors]`
layer. Both, and how they differ, are specified in
[Channel Configuration › CORS & origins](../reference/channel-config.md#cors--origins).

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

**Per-connector operation gates:** every connector type can disable operation
types in its config — making a connector read-only, insert-only, or
delete-proof regardless of what any workflow asks for. The per-type gate
vocabulary is specified in
[Connector Types › Operation gates](../reference/connectors.md#operation-gates).

**URL validation:** connector URLs are validated at creation time. Combined with SSRF protection, this prevents workflows from making requests to unexpected destinations.

**Injection protection:** JSONLogic expressions are evaluated in a sandboxed environment. User-supplied data cannot escape the data context or execute arbitrary code.
