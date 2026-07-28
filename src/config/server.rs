use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Maximum time in seconds to wait for in-flight requests during graceful shutdown.
    pub shutdown_drain_secs: u64,
    /// Upper bound in seconds on waiting for in-flight requests *after* the
    /// drain window (readiness already withdrawn, accept stopped). 0 = wait
    /// forever (pre-1.0 behavior of the plain-HTTP path).
    pub shutdown_force_timeout_secs: u64,
    /// TLS configuration for HTTPS support.
    pub tls: TlsConfig,
    /// Response compression configuration.
    pub compression: CompressionConfig,
    /// Interactive API documentation (`/docs`, `/api/v1/openapi.json`).
    pub docs: DocsConfig,
    /// Maximum request body size for the admin API, in bytes.
    ///
    /// R16: the body limit used to be one global layer set from
    /// `ingest.max_payload_size` — a name that says *data plane* — so bulk
    /// import, connector config PUTs and `POST /workflows/{id}/test` shared a
    /// ceiling with anonymous channel traffic. Raising it for a big import
    /// raised it for the unauthenticated plane too, which is the opposite of
    /// what an operator wants. The admin API is authenticated and its payloads
    /// are legitimately larger, so it gets its own bound.
    pub max_admin_body_size: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            shutdown_drain_secs: 30,
            shutdown_force_timeout_secs: 30,
            tls: TlsConfig::default(),
            compression: CompressionConfig::default(),
            docs: DocsConfig::default(),
            // 8 MB: room for a full workflow export round-trip (the largest
            // legitimate admin body) without inviting one.
            max_admin_body_size: 8 * 1_048_576,
        }
    }
}

impl ServerConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(u64::from(self.port), "server.port")?;
        require_nonzero(
            self.max_admin_body_size as u64,
            "server.max_admin_body_size",
        )?;
        if self.tls.enabled {
            require_nonempty(
                &self.tls.cert_path,
                "server.tls.cert_path (required when TLS is enabled)",
            )?;
            require_nonempty(
                &self.tls.key_path,
                "server.tls.key_path (required when TLS is enabled)",
            )?;
            if !Path::new(&self.tls.cert_path).exists() {
                return Err(OrionError::Config {
                    message: format!("TLS certificate file not found: '{}'", self.tls.cert_path),
                });
            }
            if !Path::new(&self.tls.key_path).exists() {
                return Err(OrionError::Config {
                    message: format!("TLS private key file not found: '{}'", self.tls.key_path),
                });
            }
        }
        Ok(())
    }
}

impl IngestConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(self.max_payload_size as u64, "ingest.max_payload_size")
    }
}

/// Response compression (gzip) configuration.
///
/// Disabled by default: tower-http's `CompressionLayer` is unconditional once
/// inserted and runs DEFLATE per response regardless of payload size, which
/// for small JSON responses costs CPU without saving bytes (a ~100 B response
/// can grow slightly after gzip overhead). Operators serving large responses
/// should opt in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    pub enabled: bool,
}

/// Gate for the interactive API documentation surface: Swagger UI at `/docs`
/// and the spec at `/api/v1/openapi.json` (S17).
///
/// The spec publishes the complete admin API surface — route shapes, request
/// schemas, the `admin_auth.header` semantics — and both endpoints are
/// unauthenticated, so production deployments should not serve them to
/// anonymous callers. The `dump-openapi` subcommand covers offline spec
/// generation regardless of this setting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocsConfig {
    /// Serve `/docs` and `/api/v1/openapi.json`. Unset (the default) means
    /// "enabled outside production": the same `environment` prefix rule that
    /// gates the admin-auth and CORS production checks decides. An explicit
    /// `true`/`false` always wins. When disabled the routes are not
    /// registered at all, so both paths 404.
    pub enabled: Option<bool>,
}

/// TLS configuration for HTTPS support.
/// When `enabled` is false (default), the server runs plain HTTP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Enable TLS. Requires `cert_path` and `key_path` to be set.
    pub enabled: bool,
    /// Path to the PEM-encoded certificate chain file.
    pub cert_path: String,
    /// Path to the PEM-encoded private key file.
    pub key_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IngestConfig {
    pub max_payload_size: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_payload_size: 1_048_576, // 1 MB
        }
    }
}
