<!-- description: Orion's defaults serve a laptop — admin auth off, TLS off, CORS open. What to change before anything you do not control can reach the admin or data plane. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Secure an instance

Orion's defaults serve a laptop: admin auth off, TLS off, CORS wide open, and a data plane anyone who can reach the port can call. This guide is what you change before anything you do not control can reach it.

## Before you start

You need admin access to the instance's config and the ability to restart it. Setting `environment = "production"` makes exactly five things fatal at startup rather than advisory. They are `admin_auth` disabled, an admin key too weak to be one, a `[cors] allowed_origins = ["*"]` wildcard, `server.verbose_errors = true`, and `cluster.enabled` together with `storage.auto_migrate`. Everything else on this page, TLS and per-channel data-plane `auth` included, is never gated by production mode and stays your responsibility.

## Authenticate the admin plane

`/api/v1/admin/**` and `/metrics` are unauthenticated until you turn this on. Anyone who can reach the port can rewrite your workflows:

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
read_only_api_keys = []
# header = "Authorization"    # Bearer format (the default)
# header = "X-API-Key"        # raw key format
```

- **Store digests, not keys.** A `sha256:<digest>` entry authenticates the same key without the config, the environment, or a `validate-config` dump ever holding it.
- **List more than one key.** Any match authorizes, so rotation needs no window in which requests are refused.
- **Give read-only consumers read-only keys.** A `read_only_api_keys` entry authorizes `GET` and `HEAD` and answers `403` to anything mutating. That is enough for a dashboard, an auditor or a CI check, and not enough to rewrite a workflow.

Guessing is already rate-limited. After five consecutive failures a client is put in a doubling backoff up to 30 s. Reading a trace by its token shares that budget. The policy is fixed and needs no configuration; [Failed-auth backoff](../../reference/admin-api/authentication.md#failed-auth-backoff) is the contract. Watch `orion_admin_auth_failures_total`: a sustained `invalid_key` or `locked_out` rate is someone trying.

With `header = "Authorization"` the key travels as `Bearer <key>`; any other header name takes the raw value:

```bash
curl -H "Authorization: Bearer your-secret-key" http://localhost:8080/api/v1/admin/workflows
curl -H "X-API-Key: your-secret-key"            http://localhost:8080/api/v1/admin/workflows
```

## Decide how the data plane authenticates

> [!WARNING]
> `admin_auth` does not protect `/api/v1/data/**`. A data channel is open to anything that can reach the port unless *that channel* declares an `auth` block.

Three modes are built in, all configured per channel:

- **`api_key`**: a key compared in constant time against the SHA-256 of each accepted key.
- **`hmac`**: a signature (SHA-1, SHA-256 or SHA-512) over a templated signing string, verified before parsing. The raw body is the default, and timestamped schemes come through `message` or a `preset`. This covers the webhook schemes of Stripe, GitHub, Shopify, Slack, Zoom and Webex.
- **`jwt`**: bearer-token verification, detailed below.

All three take `env://` references, mask their credential fields in API reads, and answer a uniform `401` that never reveals which part failed. The full contract, including why Kafka and `channel_call` are exempt, is [Authentication](../../reference/channel-config/auth.md).

JWT verification is built in; OIDC flows and mTLS termination are not. The `jwt` auth mode verifies bearer tokens at ingress, from static keys or a JWKS, and exposes the verified claims at `metadata.auth.claims.*`. Identity reaches the workflow without a header-forwarding proxy whose stripping rules Orion cannot validate. What stays out of scope is the identity-provider half (discovery, PKCE, userinfo) and client-certificate termination. For those, front Orion with a gateway or service mesh, and let the `jwt` mode verify what it forwards.

## Terminate TLS

Orion can terminate TLS itself, or sit behind something that does. Pick one and be deliberate about it; a plaintext listener reachable beyond the host is how admin keys leak:

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/server.crt"
key_path  = "/etc/orion/tls/server.key"
```

- **Certificates load at startup.** A missing or unreadable file is a startup failure, not a fallback to plaintext.
- **`Strict-Transport-Security` is set only when TLS is on**, so a plaintext deployment does not advertise a guarantee it cannot keep.
- **Terminating at a load balancer is equally valid.** Leave `server.tls` off, and make sure the hop between the balancer and Orion is a trusted network.

## Trust the right proxies

If anything proxies traffic to Orion, such as a load balancer, an ingress controller or a service mesh, set `rate_limit.trusted_proxies`. Do it whether or not you enable the platform rate limiter:

```toml
[rate_limit]
trusted_proxies = ["10.0.0.0/8", "fd00::/8"]
```

The rate limiter's client identity is the TCP peer address. Behind a proxy that peer is always the proxy, so every caller collapses into one bucket and real traffic starts getting `429`s. Forwarded headers are honoured only when the peer is on this list, because a client can send any header it likes. On Kubernetes this is your pod or node CIDR; behind a cloud load balancer it is the balancer's subnet.

A malformed entry fails startup even with the limiter disabled. Run `orion-server validate-config` before you deploy. The reasoning is in [Why forwarded headers are ignored by default](../../concepts/design-notes.md#why-forwarded-headers-are-ignored-by-default).

## Keep credentials out of the database

Author every connector with a reference rather than a literal:

```json
{ "config": { "type": "http", "auth": { "type": "bearer", "token": "env://STRIPE_KEY" } } }
```

References resolve at load time from the server's environment, so the stored row holds a variable name. `vault://<api-path>#<field>` reads HashiCorp Vault when `VAULT_ADDR` and `VAULT_TOKEN` are present, re-read on each reload so a rotated token applies without a restart. `aws-sm://`, `gcp-sm://` and `azure-kv://` are reserved. A reference using one without a live resolver is refused rather than passed to the backend as a literal credential.

Channel `auth` blocks take the same references, and three workflow functions take them for key material. Which fields resolve one, and which look like they should but do not, is in [Where a reference resolves](../../reference/environment-variables.md#where-a-reference-resolves).

Key material a *workflow* reads has a better home than a reference in the definition. Declare it once in `[secrets]` and name it from the task:

```toml
[secrets]
partner_hmac = "env://PARTNER_HMAC_KEY"
```

```json
{ "op": "hmac", "key": { "secret": "partner_hmac" }, "data": { "var": "data.body" } }
```

Two things change. The workflow reaches an allowlist the operator published rather than whatever the process environment holds under a name the definition chose. And a misspelled name is caught when the engine is built, not by a task failing in production. The value itself is held by the engine, never by a message. It cannot appear in a trace snapshot, a `map` mapping clone or a response body. The engine refuses a workflow that would copy it somewhere recorded. See [Vars and secrets](../../reference/configuration/vars-and-secrets.md).

For defence below the API, encrypt the connector configs at rest:

```toml
[storage]
connector_encryption_key = "env://ORION_SECRET_CONNECTOR_KEY"
```

A database dump then carries an opaque envelope (AES-256-GCM) instead of credentials.

Reads of a connector are masked by allowlist. Only the structural vocabulary each type defines comes back readable, and everything else is `"******"`. A credential under an unanticipated key fails closed rather than shipping in clear. The normative rules are [Secret masking](../../reference/connectors/masking.md).

### What a failed call may repeat back

An error from `http_call` names the endpoint it could not reach, because that is the diagnostic. Two things bound what that costs you.

**URLs are redacted where they appear.** The userinfo password and any query value whose name reads as a secret (`pwd`, `api_key`, `sig`, …) are masked. That happens before the URL reaches an error message, a log line, an OTel span, a trace row or the DLQ. The match is by parameter name, so it closes the conventional spellings and not an unconventional one: `?pwd=` masks, `?pass=` does not. Treat it as a backstop, not the control. The control is not putting the credential in the URL: use `auth`, or `query_params`, whose values resolve from references and never enter the URL string.

**Upstream error bodies are previewed, not copied.** A non-2xx response contributes at most the first 512 bytes of its body to the error message, marked `… (truncated)` when it is cut. Anything a failing API echoes back, such as a token, an account record or a stack trace, is bounded. It is not persisted whole into `traces` and `trace_dlq`. This limit is separate from and much smaller than `max_response_size`, which governs the body a *successful* call may return to the workflow.

Both matter because these strings outlive the request. They are persisted to the trace, and an async caller can read its own trace back with the `trace_token` returned by the `202`. An admin credential is not the only key to them. If a connector must carry a secret an error could name, keep it out of the URL rather than relying on redaction to catch it.

## Bound what connectors can reach

Connectors are refused an endpoint that could not belong to their backend, and refused a private address unless you say otherwise. Two layers:

| Layer | Runs at | Checks |
|---|---|---|
| Scheme allow-list | create / update | The endpoint's scheme suits its backend; a `db` connector cannot hold `http://169.254.169.254/…` |
| Private-address check | first connection | The resolved address is not RFC 1918, loopback, link-local, CGNAT, or the cloud metadata range |

Say so explicitly when a private address is intended:

```json
{ "config": { "type": "db", "connection_string": "env://ORDERS_DB_URL", "allow_private_urls": true } }
```

Most databases and caches *are* private, so most deployments set `allow_private_urls: true` on them. That is the intended outcome. The flag makes reaching an internal address a stated decision instead of the default, which keeps the unstated case, a workflow-authored connector reaching `169.254.169.254`, refused. Because the driver re-resolves the hostname when it dials, this is a guard rather than a guarantee. Pair it with network-level egress policy where the difference matters.

Then bound what workflows may *do* through a connector with its operation gates, which are enforced at the connection regardless of what any workflow asks:

```json
{ "operations": { "delete": false, "raw_write": false } }
```

See [Operation gates](../../reference/connectors/operation-gates.md).

## Check origins server-side

Two different things share the word "origin", and conflating them is the common mistake:

- **`origin_allow_list`** is a per-channel server-side check. A request whose `Origin` header is not listed is refused. It is enforcement.
- **`[cors]`** is the platform's browser handshake. It tells a browser what it may do. It is not enforcement; a non-browser client ignores it entirely.

Set the first when a channel should only serve named origins. Set the second so browsers behave. Neither is authentication, because `Origin` is client-supplied. The per-channel check is specified in [CORS and origins](../../reference/channel-config/origin_allow_list.md); the instance-level handshake is [Configuration › CORS](../../reference/configuration/cors.md).

Credentialed CORS widens what a browser does on a user's behalf. `cors.allow_credentials = true` lets any page on a listed origin send the user's cookies to Orion and read the response. The origin list becomes a trust boundary, not a convenience. Two consequences follow:

- Every origin you list is one that can act as a logged-in user. List the applications you operate, never a wildcard subdomain or a CDN you share.
- Orion refuses `allow_credentials` together with `allowed_origins = ["*"]` at startup. Browsers reject the combination anyway, and the underlying layer asserts on it at router construction, so the alternative is a process that crashes at boot.

Credentialed cross-origin sessions usually also need `set-cookie` in `cors.additional_exposed_headers` before a page script can see it.

## Bound what a plugin can do

A [plugin](../../concepts/plugins.md) is code you did not write running inside the server, so its security model is worth stating exactly. Three things are true by construction, and two are deliberately not claimed.

**By construction.** The WebAssembly world a plugin implements imports nothing: no filesystem, clock, randomness, sockets, logging, connectors, secrets or task context. Reads arrive only through the task's evaluated input; writes leave only through the return value, which the host writes at one `output` path. Every invocation runs in a fresh instance under the node's ceilings, set in [`[plugins]`](../../reference/configuration/plugins.md). Those are linear memory, wall clock, input and output size, concurrency per function, and a fuel backstop. A per-plugin override may only lower one. A failure of any kind writes nothing; a trapped instance is dropped, so no guest state crosses messages. Guest strings never reach a metric label, and a trap's internals go to the operator log, never to a client. A `{"secret": …}` node is refused anywhere in a plugin task's input at create time, so a template field cannot evaluate a secret into the sandbox.

**Who may install one.** The admin credential, the one that already reads and writes connector secrets, so a plugin adds no new principal. The optional hardening on top is [`[plugins.trust]`](../../reference/plugin-manifest.md#trust). When it names Ed25519 public keys, an upload must carry a signature over the component digest by one of them. Every node verifies it again when it loads the version. Leave `plugins.enabled = false` on any node that should never run one; a stored plugin then quarantines the workflows naming its functions rather than running.

**Not claimed.** That a malicious plugin is harmless. Wasmtime, Cranelift and the component toolchain join the trusted computing base, and the update policy for them is in `SECURITY.md`. Or that the sandbox sees a wrong answer. It bounds blast radius, not truth, and a codec that mis-parses a field mis-parses it in a sandbox. Review a plugin's source as you would a connector's credentials.

## Close the surfaces you do not need

- **Swagger UI and the OpenAPI spec** publish the complete admin API to anonymous callers. `server.docs.enabled` unset serves them only outside production; set it `false` to be explicit. `orion-server dump-openapi` still writes the spec offline.
- **`/metrics` is admin-authenticated** along with the rest of the admin plane, and that credential can also rewrite workflows. Give the scraper its own listener instead. `metrics.bind_addr = "127.0.0.1:9090"` moves the endpoint to a separate unauthenticated port where the address *is* the access control.
- **Payload size** is capped by `ingest.max_payload_size`, 1 MB by default. Raise it deliberately; it is the bound on what one request can cost you.

## Verify

Check the config, then prove the admin plane refuses an anonymous caller:

```bash
orion-server validate-config -c config.toml
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8080/api/v1/admin/workflows
```

The first prints the effective config with secrets masked and refuses a malformed proxy entry. The second prints `401`. With TLS on, `curl -sI https://…` shows a `strict-transport-security` header.

## Next steps

- [Production checklist](../production-checklist.md): every pre-go-live item, including the ones on this page, as one list.
- [Run a cluster](../deploy/cluster.md): what changes about all of this once there is more than one replica.
- [Channel configuration](../../reference/channel-config/index.md): the per-channel `auth`, `origin_allow_list` and validation contracts.
- [Connector types](../../reference/connectors/index.md): secret references, masking and operation gates in full.
- [Server configuration](../../reference/configuration/index.md): every key named here, with its default and environment variable.
