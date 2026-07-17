# Security

Orion enforces security at every layer: secrets are isolated in connectors, inputs are validated before processing, network requests are checked for SSRF, and admin endpoints are protected by authentication.

## Secret Management

Sensitive fields are automatically masked in all API responses. Fields named `token`, `password`, `key`, `secret`, `api_key`, and `connection_string` are returned as `"******"`. Secrets are stored but never exposed through the API.

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

**SSRF protection:** HTTP connectors validate URLs to prevent Server-Side Request Forgery. By default, requests to private/internal IP addresses (RFC 1918, loopback, link-local) are blocked:

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

Set `allow_private_urls: true` only when calling internal services.

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
# header = "Authorization"       # Bearer format (default)
# header = "X-API-Key"           # Raw key format
```

When `header` is `"Authorization"`, the key is expected as `Bearer <key>`. For any other header name, the raw key value is matched directly.

```bash
# Bearer token
curl -H "Authorization: Bearer your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows

# API key via custom header
curl -H "X-API-Key: your-secret-key" \
  http://localhost:8080/api/v1/admin/workflows
```

**Per-channel CORS:** configure allowed origins per channel in `config_json`:

```json
{
  "cors": {
    "allowed_origins": ["https://app.example.com", "https://admin.example.com"]
  }
}
```

Global CORS defaults are configured in the server config:

```toml
[cors]
allowed_origins = ["*"]    # Global default
```

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
come only from the envelope and schema and are quoted per dialect. An optional
schema allowlist (`"unmapped": "reject"`) restricts which entities and columns
workflows can touch, with per-column `queryable`/`writable` flags. See the
[Portable Data Dialect](../reference/data-dialect.md) reference.

**Write-safety guards:** an unfiltered `data_write` update/delete is rejected
unless the call carries `"all": true` **and** the server enables
`write.allow_unfiltered` — a double opt-in against accidental table truncation.
Bulk inserts over `write.max_rows` are rejected, never silently truncated.

**Per-connector operation gates:** a `db` or `es` connector's config can
disable operation types outright — `operations: { "read", "insert", "update",
"delete", "upsert", "raw_write" }`, all defaulting to allowed. A gated call
fails with a validation error naming the op and connector, so a connector can
be made read-only (or insert-only, delete-proof, …) in configuration,
regardless of what any workflow asks for:

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

**URL validation:** connector URLs are validated at creation time. Combined with SSRF protection, this prevents workflows from making requests to unexpected destinations.

**Injection protection:** JSONLogic expressions are evaluated in a sandboxed environment. User-supplied data cannot escape the data context or execute arbitrary code.
