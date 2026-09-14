<!-- description: The [logging] and [metrics] settings: log level and format, enabling Prometheus metrics, and the dedicated unauthenticated metrics listener. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Logging and metrics settings

The `[logging]` and `[metrics]` sections: log level and format, and where Prometheus metrics are served.

## Synopsis

```toml
[logging]
level = "info"
format = "pretty"

[metrics]
enabled = false
# bind_addr = …   # no default
```

## Description

That listener is plain HTTP (`server.tls` governs the main listener only) and has no authentication by design. The address is the access control, so bind it somewhere only your scrapers can reach. Startup logs a warning if it is not a loopback address. It logs another if `bind_addr` is set while `metrics.enabled` is `false`, because that combination serves `/metrics` nowhere at all. It joins the same graceful-shutdown path as the main
listener, keeping the last scrape of a draining node available for
`server.shutdown_drain_secs`.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `logging.level` | `"info"` | `ORION_LOGGING__LEVEL` | `trace`, `debug`, `info`, `warn`, `error`. `RUST_LOG=orion=debug` gives per-crate control. |
| `logging.format` | `"pretty"` | `ORION_LOGGING__FORMAT` | `json` wherever logs are collected by anything other than a human. |
| `metrics.enabled` | `false` | `ORION_METRICS__ENABLED` | Enable to collect metrics and serve them at `GET /metrics`. With this off the route is not registered at all: `/metrics` answers `404` rather than `200` with a permanently empty body. |
| `metrics.bind_addr` | — | `ORION_METRICS__BIND_ADDR` | A dedicated `host:port` for an **unauthenticated** listener serving only `GET /metrics`. Unset keeps the endpoint on the main listener, where `admin_auth` guards it. Requires `metrics.enabled = true`; set on its own it raises no listener and startup warns. Refused at startup if it would contend with `server.host`/`server.port` — same port counts as contention whenever either side is a wildcard address. |

**Where `/metrics` is served.** By default it lives on the main listener. With `admin_auth.enabled = true` every scraper must then hold an admin API key, a credential that can also rewrite workflows and read trace payloads. Setting
`metrics.bind_addr` moves the endpoint onto its own listener, removes it from
the main one entirely, and drops the credential requirement:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"     # or a pod IP / a private Compose network
```

## Related

- [Monitor and alert](../../operate/run/monitoring.md): scraping `/metrics` and alerting on it.
- [Metrics](../metrics.md): every series the listener serves.
- [Secure an instance](../../operate/run/security.md): why the metrics listener has no credential.
- [Server configuration](./index.md): every section, by what you are configuring.
