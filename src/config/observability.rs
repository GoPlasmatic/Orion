use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Collect and expose Prometheus metrics. When `false` nothing is
    /// recorded **and** `/metrics` is not registered at all — the path 404s
    /// like any other unknown route (O12). It used to be registered
    /// unconditionally and answer 200 with an empty body rendered from an
    /// orphan recorder, so a deployment with metrics off looked like a
    /// working scrape target that simply never had any series.
    pub enabled: bool,

    /// Optional `host:port` for a dedicated, **unauthenticated** listener
    /// serving only `GET /metrics` (O12).
    ///
    /// Unset (the default) keeps `/metrics` on the main listener, where
    /// `admin_auth` guards it — so every scraper has to hold an admin API
    /// key, a credential that can also rewrite workflows and read trace
    /// payloads. Set this to a private interface (`127.0.0.1:9090`, a pod IP,
    /// a `metrics` network in Compose) and the scraper needs no credential at
    /// all, while the main listener stops serving `/metrics` entirely.
    ///
    /// Plain HTTP only: `server.tls` applies to the main listener. Bind it
    /// somewhere a TLS-terminating hop is not required — startup logs a
    /// warning if the address is not loopback.
    pub bind_addr: Option<String>,
}

impl MetricsConfig {
    pub(crate) fn validate(&self, server: &crate::config::ServerConfig) -> Result<(), OrionError> {
        let Some(addr) = self.bind_addr.as_deref() else {
            return Ok(());
        };
        let parsed = addr
            .parse::<std::net::SocketAddr>()
            .map_err(|e| OrionError::Config {
                message: format!(
                    "metrics.bind_addr '{addr}' is not a valid host:port address: {e}"
                ),
            })?;
        // Two listeners on one address is a boot-time bind failure at best and,
        // with SO_REUSEADDR set on both sockets, a platform-dependent split of
        // incoming connections at worst. Say so here instead.
        if parsed.port() == server.port && Self::hosts_overlap(&server.host, parsed.ip()) {
            return Err(OrionError::Config {
                message: format!(
                    "metrics.bind_addr '{addr}' overlaps server.host/server.port \
                     ('{}:{}') — the metrics listener needs an address of its own \
                     (leave it unset to keep /metrics on the main listener)",
                    server.host, server.port
                ),
            });
        }
        Ok(())
    }

    /// Whether a metrics listener on `metrics_ip` would contend with a main
    /// listener on `server_host`, both on the same port.
    ///
    /// Exact equality is not enough. `create_tcp_listener` sets
    /// `SO_REUSEADDR` on both sockets and the metrics listener binds first, so
    /// on BSD/macOS `server.host = "0.0.0.0"` plus
    /// `metrics.bind_addr = "127.0.0.1:8080"` both bind successfully and the
    /// more specific socket captures every loopback connection to the main
    /// port — precisely the split this check exists to prevent. A wildcard on
    /// either side therefore covers the other.
    ///
    /// `server.host` may also be a hostname (`localhost`, a service name), in
    /// which case there is nothing to compare and the previous check silently
    /// passed everything. Treat that as overlapping: sharing a port with a
    /// host this process cannot resolve here is not something to guess at.
    fn hosts_overlap(server_host: &str, metrics_ip: std::net::IpAddr) -> bool {
        let Ok(server_ip) = server_host.parse::<std::net::IpAddr>() else {
            return true;
        };
        server_ip.is_unspecified() || metrics_ip.is_unspecified() || server_ip == metrics_ip
    }

    /// True when the *main* router should register `/metrics`: collection is
    /// on and no dedicated listener has claimed the endpoint.
    pub fn on_main_listener(&self) -> bool {
        self.enabled && self.bind_addr.is_none()
    }

    /// The dedicated listener address, only when metrics are actually
    /// collected — `bind_addr` with `enabled = false` would serve an empty
    /// body forever, which is the O12 defect one interface over.
    pub fn dedicated_bind_addr(&self) -> Option<&str> {
        self.enabled.then_some(self.bind_addr.as_deref()).flatten()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TracingConfig {
    /// Enable OpenTelemetry trace export at runtime. Compiled into every build.
    pub enabled: bool,
    /// OTLP gRPC endpoint (e.g. Jaeger, Grafana Tempo, OTel Collector).
    pub otlp_endpoint: String,
    /// Service name reported in traces.
    pub service_name: String,
    /// Sampling rate from 0.0 (none) to 1.0 (all).
    pub sample_rate: f64,
    /// Allow per-request workflow profiling. When `true`, requests carrying
    /// `X-Orion-Profile: 1` (or `?profile=1`) receive a `profile` object in
    /// the response that breaks the request down by phase (engine lock,
    /// per-handler durations, trace store, residual workflow logic).
    ///
    /// Default `false` — the header is ignored in production until this is
    /// switched on, so attackers cannot probe internal timing.
    pub debug_profile_enabled: bool,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "orion".to_string(),
            sample_rate: 1.0,
            debug_profile_enabled: false,
        }
    }
}

impl TracingConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if self.enabled {
            require_nonempty(
                &self.otlp_endpoint,
                "tracing.otlp_endpoint (required when tracing is enabled)",
            )?;
            if !(0.0..=1.0).contains(&self.sample_rate) {
                return Err(OrionError::Config {
                    message: "tracing.sample_rate must be between 0.0 and 1.0".to_string(),
                });
            }
        }
        Ok(())
    }
}

impl TraceStorageConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if !(0.0..=1.0).contains(&self.sample_rate) {
            return Err(OrionError::Config {
                message: "trace_storage.sample_rate must be between 0.0 and 1.0".to_string(),
            });
        }
        match self.mode {
            TraceStorageMode::Async => {
                require_nonzero(self.max_pending as u64, "trace_storage.max_pending")?;
                require_nonzero(self.async_workers as u64, "trace_storage.async_workers")?;
            }
            TraceStorageMode::Batch => {
                require_nonzero(self.max_pending as u64, "trace_storage.max_pending")?;
                require_nonzero(self.batch_size as u64, "trace_storage.batch_size")?;
                // Q8: the batch INSERT binds ~11 parameters per row and
                // SQLite caps a statement at 32 766 binds — batch_size 3000
                // made every flush fail, and the whole batch was discarded.
                // 1000 rows ≈ 11 000 binds leaves comfortable headroom on
                // every backend.
                if self.batch_size > 1000 {
                    return Err(OrionError::Config {
                        message: "trace_storage.batch_size must be <= 1000 (the batch \
                                  INSERT binds ~11 parameters per row and SQLite caps a \
                                  statement at 32 766 binds)"
                            .to_string(),
                    });
                }
                require_nonzero(
                    self.batch_flush_interval_ms,
                    "trace_storage.batch_flush_interval_ms",
                )?;
                require_nonzero(self.batch_workers as u64, "trace_storage.batch_workers")?;
            }
            TraceStorageMode::Sync | TraceStorageMode::Off => {}
        }
        Ok(())
    }
}

/// Persistence mode for engine traces.
///
/// `Sync` writes inside the request path: strongest durability, and throughput
/// capped by single-writer DB contention. `Async` enqueues to a bounded
/// background queue, one DB write per task. `Batch` is the throughput-optimised
/// path and the default: background workers accumulate writes and commit them
/// in one transaction. `Off` disables persistence entirely.
///
/// `Batch` is the default because `Sync` makes a trace write part of answering
/// a request — on the default SQLite backend, a single-writer fsync per request
/// on the hottest path in the product. Traces are observability data about a
/// request, not part of its result, so the failure mode that belongs to them is
/// losing a window of traces under overload rather than slowing every caller
/// down to the speed of the trace table.
///
/// What the default costs: a *hard* kill can lose up to
/// `batch_flush_interval_ms` of traces, and a sustained overrun of `max_pending`
/// drops them (`async_on_overflow`). Graceful shutdown drains the queue, so an
/// orderly restart loses nothing. Deployments that treat the trace table as an
/// audit record rather than as telemetry should set `mode = "sync"` — the
/// `audit_logs` table is unaffected either way and remains the durable record of
/// admin mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TraceStorageMode {
    Sync,
    Async,
    /// Default: keeps trace persistence off the request path.
    #[default]
    Batch,
    Off,
}

/// Policy for the persistence queue when full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AsyncOnOverflow {
    /// Drop the trace; increment `trace_dropped_total{reason="overflow"}`.
    #[default]
    Drop,
    /// Wait up to `overflow_block_timeout_ms` for capacity, then drop.
    Block,
}

// `PartialEq` is load-bearing: `ChannelRegistry` keys its per-channel runtime
// cache on the global trace-storage config these values resolve against (N17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TraceStorageConfig {
    /// Persistence policy. Applies to `store_completed` (sync result write)
    /// and `set_result` / `update_status` (async result writes). The async
    /// endpoint's `create_pending` step is always synchronous so the
    /// `GET /traces/{id}` contract is preserved after a 202.
    pub mode: TraceStorageMode,

    // ---- Filters (compose with mode; applied per trace) ----
    /// Fraction of traces to persist, 0.0–1.0. Roll a coin per trace; on a
    /// failed roll the trace is treated as `Off` and recorded in
    /// `trace_dropped_total{reason="sampled_out"}`.
    pub sample_rate: f64,

    /// When true, only persist traces that ended with errors
    /// (`message.has_errors()` for sync, `error_message.is_some()` for async).
    /// Successful traces are dropped with `reason="errors_only"`.
    pub errors_only: bool,

    // ---- Async / batch queue knobs ----
    /// Bounded mpsc capacity for the persistence queue.
    pub max_pending: usize,

    /// Behaviour when the queue is full (`async` and `batch` modes only).
    pub async_on_overflow: AsyncOnOverflow,

    /// When `async_on_overflow = "block"`, the producer waits at most this
    /// many milliseconds for capacity before dropping the trace.
    pub overflow_block_timeout_ms: u64,

    // ---- Async-mode-specific ----
    /// Worker count for `async` mode (one DB write per worker iteration).
    pub async_workers: usize,

    // ---- Batch-mode-specific ----
    /// Maximum entries accumulated before forcing a batch flush.
    pub batch_size: usize,

    /// Maximum time to wait before flushing a non-full batch (milliseconds).
    pub batch_flush_interval_ms: u64,

    /// Worker count for `batch` mode (each worker owns an independent batch).
    pub batch_workers: usize,
}

impl Default for TraceStorageConfig {
    fn default() -> Self {
        Self {
            mode: TraceStorageMode::Batch,
            sample_rate: 1.0,
            errors_only: false,
            max_pending: 10_000,
            async_on_overflow: AsyncOnOverflow::Drop,
            overflow_block_timeout_ms: 100,
            async_workers: 4,
            batch_size: 100,
            batch_flush_interval_ms: 100,
            batch_workers: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CorsConfig {
    /// Allowed origins. Use `["*"]` (default) for permissive CORS.
    pub allowed_origins: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_string()],
        }
    }
}

impl CorsConfig {
    pub(crate) fn validate(&self, is_production: bool) -> Result<(), OrionError> {
        // A "*" mixed into an explicit origin list would skip the permissive
        // branch in build_cors and reach AllowOrigin::list, which panics on
        // wildcard entries — a boot-time crash from config alone.
        if self.allowed_origins.len() > 1 && self.allowed_origins.iter().any(|o| o == "*") {
            return Err(OrionError::Config {
                message: "CORS allowed_origins cannot mix '*' with explicit origins. \
                          Use exactly [\"*\"] for permissive CORS, or list explicit origins only"
                    .to_string(),
            });
        }
        if self.allowed_origins.len() == 1 && self.allowed_origins[0] == "*" {
            if is_production {
                return Err(OrionError::Config {
                    message:
                        "CORS wildcard '*' is not allowed when environment starts with 'prod'. \
                         Set explicit origins in [cors] allowed_origins"
                            .to_string(),
                });
            }
            tracing::warn!(
                "CORS is set to permissive ('*'). For production, configure specific origins in [cors] allowed_origins"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    // -- metrics listener (O12) ------------------------------------------

    fn metrics(bind_addr: Option<&str>) -> MetricsConfig {
        MetricsConfig {
            enabled: true,
            bind_addr: bind_addr.map(str::to_string),
        }
    }

    #[test]
    fn bind_addr_must_be_a_host_port_pair() {
        let err = metrics(Some("not-an-address"))
            .validate(&ServerConfig::default())
            .expect_err("a bad address must fail at startup, not at bind");
        assert!(err.to_string().contains("metrics.bind_addr"), "{err}");
        // A bare port is the likely typo and must not silently mean "any host".
        assert!(
            metrics(Some("9090"))
                .validate(&ServerConfig::default())
                .is_err()
        );
        assert!(
            metrics(Some("127.0.0.1:9090"))
                .validate(&ServerConfig::default())
                .is_ok()
        );
    }

    #[test]
    fn bind_addr_must_not_collide_with_the_main_listener() {
        let server = ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            ..ServerConfig::default()
        };
        let err = metrics(Some("127.0.0.1:8080"))
            .validate(&server)
            .expect_err("two listeners on one address must be refused");
        assert!(err.to_string().contains("overlaps"), "{err}");
        assert!(metrics(Some("127.0.0.1:9090")).validate(&server).is_ok());
    }

    /// The case exact `SocketAddr` equality let through: with `SO_REUSEADDR`
    /// on both sockets and the metrics listener bound first, a specific
    /// address alongside a wildcard is accepted by the OS and the specific
    /// socket then swallows that interface's traffic to the main port.
    #[test]
    fn a_wildcard_on_either_side_overlaps_the_same_port() {
        let wildcard = |host: &str| ServerConfig {
            host: host.to_string(),
            port: 8080,
            ..ServerConfig::default()
        };
        for host in ["0.0.0.0", "::"] {
            let err = metrics(Some("127.0.0.1:8080"))
                .validate(&wildcard(host))
                .expect_err("a wildcard server.host must overlap a specific metrics address");
            assert!(err.to_string().contains("overlaps"), "{err} (host {host})");
        }
        // ...and the mirror image: a wildcard metrics listener over a
        // specific main listener.
        assert!(
            metrics(Some("0.0.0.0:8080"))
                .validate(&ServerConfig {
                    host: "10.0.0.5".to_string(),
                    port: 8080,
                    ..ServerConfig::default()
                })
                .is_err()
        );
        // Distinct interfaces on the same port genuinely do not contend.
        assert!(
            metrics(Some("127.0.0.1:8080"))
                .validate(&ServerConfig {
                    host: "10.0.0.5".to_string(),
                    port: 8080,
                    ..ServerConfig::default()
                })
                .is_ok()
        );
    }

    /// A hostname cannot be compared here, so sharing a port with one is
    /// refused rather than waved through — the old check degraded to a no-op
    /// the moment `server.host` was not a literal address.
    #[test]
    fn an_unresolvable_server_host_on_the_same_port_is_refused() {
        let server = ServerConfig {
            host: "localhost".to_string(),
            port: 8080,
            ..ServerConfig::default()
        };
        assert!(metrics(Some("127.0.0.1:8080")).validate(&server).is_err());
        assert!(metrics(Some("127.0.0.1:9090")).validate(&server).is_ok());
    }

    #[test]
    fn registration_follows_enabled_and_bind_addr() {
        // Off: nowhere. On + unset: the main listener. On + set: the dedicated
        // one, and *only* the dedicated one.
        let off = MetricsConfig::default();
        assert!(!off.on_main_listener());
        assert_eq!(off.dedicated_bind_addr(), None);

        let main_only = metrics(None);
        assert!(main_only.on_main_listener());
        assert_eq!(main_only.dedicated_bind_addr(), None);

        let dedicated = metrics(Some("127.0.0.1:9090"));
        assert!(
            !dedicated.on_main_listener(),
            "a dedicated listener moves the endpoint, it does not duplicate it"
        );
        assert_eq!(dedicated.dedicated_bind_addr(), Some("127.0.0.1:9090"));

        // A bind_addr with collection off must not raise a listener that could
        // only ever serve an empty body.
        let disabled_but_bound = MetricsConfig {
            enabled: false,
            bind_addr: Some("127.0.0.1:9090".to_string()),
        };
        assert_eq!(disabled_but_bound.dedicated_bind_addr(), None);
        assert!(!disabled_but_bound.on_main_listener());
    }

    #[test]
    fn batch_size_is_bounded_against_sqlite_bind_limit() {
        // Q8: >1000 rows would exceed SQLITE_MAX_VARIABLE_NUMBER at ~11
        // binds per row, making every flush fail (and, before Q6, silently
        // discard the batch).
        let config = TraceStorageConfig {
            mode: TraceStorageMode::Batch,
            batch_size: 1001,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = TraceStorageConfig {
            mode: TraceStorageMode::Batch,
            batch_size: 1000,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }
}
