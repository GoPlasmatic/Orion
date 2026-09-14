<!-- description: The [server] settings: bind address and port, shutdown timeouts, the admin body limit, data mounts, verbose errors, TLS, compression and the API docs. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Server settings

The `[server]` section: the HTTP listener, shutdown timing, the admin body limit, extra data-plane mounts, verbose errors, TLS, compression and the API docs.

## Synopsis

```toml
[server]
host = "0.0.0.0"
port = 8080
shutdown_drain_secs = 30
shutdown_force_timeout_secs = 30
max_admin_body_size = 8388608
data_mounts = []

[server.tls]
enabled = false
cert_path = ""
key_path = ""

[server.compression]
enabled = false

[server.docs]
# enabled = …   # no default
```

## Description

Both endpoints are unauthenticated, and the spec publishes the complete admin API surface: route shapes, request schemas, the `admin_auth.header` semantics. Production deployments therefore do not serve them by default. When disabled the routes are not registered at all: both paths return 404, not 401, so their existence is not advertised. `orion-server dump-openapi` writes the spec to a file offline regardless of this setting.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.host` | `"0.0.0.0"` | `ORION_SERVER__HOST` | Bind to `127.0.0.1` when a local proxy is the only intended client. |
| `server.port` | `8080` | `ORION_SERVER__PORT` | To fit an existing port convention. |
| `server.shutdown_drain_secs` | `30` | `ORION_SERVER__SHUTDOWN_DRAIN_SECS` | Raise it if your slowest request legitimately outlives 30 s, so rolling deploys stop cutting them off. |
| `server.shutdown_force_timeout_secs` | `30` | `ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS` | Hard cap on waiting after the drain window. `0` waits forever — only with an orchestrator that will eventually SIGKILL. |
| `server.max_admin_body_size` | `8388608` | `ORION_SERVER__MAX_ADMIN_BODY_SIZE` | Raise for bulk imports or workflow exports above 8 MiB. Applies to `/api/v1/admin/*` only; the data plane keeps `ingest.max_payload_size`. |
| `server.data_mounts` | `[]` | `ORION_SERVER__DATA_MOUNTS` | Serve the data plane at extra path prefixes, for deployed clients that call legacy paths. Comma-separated in the env var. |

**`data_mounts` is additive.** `/api/v1/data` stays mounted, so every existing client and `orion-cli` command keeps working — this is not a movable prefix. A channel's `route_pattern` is unchanged; it is also served under each mount:

```toml
[server]
data_mounts = ["/zoom", "/Legacy-App"]
```

A channel with `route_pattern = "/zoom/meetings/user"` then answers at both `/zoom/meetings/user` and `/api/v1/data/zoom/meetings/user`. `/async` works under a mount too.

A mount may not claim a platform route: `/api`, `/health`, `/healthz`, `/readyz`, `/metrics` or `/docs`. The `/api` prefix covers `/api/v1/admin`, `/api/v1/data`, the OpenAPI document and any future `/api/v2`. Two mounts may not nest, which would be a router conflict at boot. All of these are startup errors.

> [!WARNING]
> The literal `"/"` mounts the data plane at the root. It is accepted, and it is the blunt option: an unmatched URL becomes a channel lookup instead of a `404`, and a **future platform route could shadow a channel already serving that path**: a wrong answer rather than an error. Orion refuses to activate a channel whose served path would fall under a platform route, and warns at startup, but a named mount avoids the hazard entirely by claiming a first-segment namespace. Prefer one.

| `server.verbose_errors` | — | `ORION_SERVER__VERBOSE_ERRORS` | Unset returns real task-failure messages on the data plane when `environment` is not a production variant, and the generic placeholder in it. `false` sanitizes everywhere. **`true` is refused in production** and the server does not start. |

On SIGTERM or SIGINT Orion withdraws readiness first, then stops accepting, then drains. Set both timeouts below your orchestrator's termination grace period, or it kills the process mid-drain.

A failed task answers `200` with the failure in the envelope's `errors` array, carrying the task's `code` and `task_id` either way. `verbose_errors` decides only whether `message` is the engine's own text or the placeholder. Full detail
is always in the persisted trace, correlated by the `request_id` the sanitized
envelope adds.

### TLS

Terminate HTTPS in Orion itself, or leave this off when a load balancer or service mesh already terminates TLS.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.tls.enabled` | `false` | `ORION_SERVER__TLS__ENABLED` | Enable when Orion is directly reachable by clients. |
| `server.tls.cert_path` | `""` | `ORION_SERVER__TLS__CERT_PATH` | PEM certificate chain. Required when enabled. |
| `server.tls.key_path` | `""` | `ORION_SERVER__TLS__KEY_PATH` | PEM private key. Required when enabled. |

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/tls.crt"
key_path = "/etc/orion/tls/tls.key"
```

Both files must exist and be readable at startup; Orion refuses to boot otherwise rather than falling back to plain HTTP.

### Compression

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.compression.enabled` | `false` | `ORION_SERVER__COMPRESSION__ENABLED` | Enable when responses are typically large. |

Off by default because the layer is unconditional once inserted. It runs DEFLATE on every response regardless of size, which costs CPU without saving bytes on small JSON bodies. A ~100 B response can grow slightly after gzip overhead.

### API docs

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `server.docs.enabled` | — | `ORION_SERVER__DOCS__ENABLED` | Unset serves Swagger UI (`/docs`) and the spec (`/api/v1/openapi.json`) only when `environment` is not a production variant. Set `true` to serve them in production anyway, `false` to switch them off everywhere. |

## Related

- [Secure an instance](../../operate/run/security.md): TLS, trusted proxies and the admin plane.
- [Deploy with Docker](../../operate/deploy/docker.md): the ports and volumes a container exposes.
- [Data API](../data-api.md): the data plane `data_mounts` re-serves.
- [Server configuration](./index.md): every section, by what you are configuring.
