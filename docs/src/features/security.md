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
api_key = "your-secret-key"
# header = "Authorization"      # Bearer format (default)
# header = "X-API-Key"          # Raw key format
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

**URL validation:** connector URLs are validated at creation time. Combined with SSRF protection, this prevents workflows from making requests to unexpected destinations.

**Injection protection:** JSONLogic expressions are evaluated in a sandboxed environment. User-supplied data cannot escape the data context or execute arbitrary code.
