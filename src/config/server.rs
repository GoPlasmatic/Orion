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
    /// Return real task-failure messages on the data plane instead of the
    /// generic placeholder.
    ///
    /// Unset (the default) means "on outside production", the same
    /// `environment` prefix rule that gates [`DocsConfig::enabled`]. Read
    /// through [`crate::config::AppConfig::verbose_errors`], never directly —
    /// the `Option` is the authored value, not the effective one.
    ///
    /// Sanitizing unconditionally is right for production (G1: raw engine
    /// messages can carry upstream URLs, connector names and driver errors,
    /// and the data plane is unauthenticated) and wrong for development, where
    /// it costs a round trip to the trace API to learn what a task did. An
    /// explicit `true` in production is refused at startup rather than
    /// honoured — see [`ServerConfig::validate`].
    pub verbose_errors: Option<bool>,
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
            verbose_errors: None,
            // 8 MB: room for a full workflow export round-trip (the largest
            // legitimate admin body) without inviting one.
            max_admin_body_size: 8 * 1_048_576,
        }
    }
}

impl ServerConfig {
    pub(crate) fn validate(&self, is_prod: bool) -> Result<(), OrionError> {
        // Refused rather than downgraded to a warning, for the same reason the
        // CORS wildcard and a missing production `admin_auth` are: the data
        // plane is unauthenticated, so honouring this would publish connector
        // names, upstream URLs and driver errors to anonymous callers. Leaving
        // it unset already does the right thing in both environments, so an
        // explicit `true` here is a mistake rather than an informed choice.
        if is_prod && self.verbose_errors == Some(true) {
            return Err(OrionError::Config {
                message: "server.verbose_errors = true is refused in production: raw \
                          task errors can carry upstream URLs, connector names and \
                          driver detail, and the data plane is unauthenticated. Leave \
                          it unset (verbose outside production, sanitized in it) and \
                          read full messages from the trace"
                    .to_string(),
            });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Unset is the supported way to get sanitized production errors, so it
    /// must not be what the production check trips on.
    #[test]
    fn verbose_errors_unset_is_allowed_in_production() {
        let config = ServerConfig::default();
        assert_eq!(config.verbose_errors, None);
        assert!(config.validate(true).is_ok());
    }

    /// Explicitly sanitizing everywhere is a legitimate choice.
    #[test]
    fn verbose_errors_false_is_allowed_in_production() {
        let config = ServerConfig {
            verbose_errors: Some(false),
            ..ServerConfig::default()
        };
        assert!(config.validate(true).is_ok());
    }

    /// Outside production an explicit `true` is just the default, restated.
    #[test]
    fn verbose_errors_true_is_allowed_outside_production() {
        let config = ServerConfig {
            verbose_errors: Some(true),
            ..ServerConfig::default()
        };
        assert!(config.validate(false).is_ok());
    }

    /// The combination that would publish connector names and driver detail to
    /// an unauthenticated data plane refuses at startup rather than warning.
    #[test]
    fn verbose_errors_true_is_refused_in_production() {
        let config = ServerConfig {
            verbose_errors: Some(true),
            ..ServerConfig::default()
        };
        let err = config
            .validate(true)
            .expect_err("verbose errors in production must not start");
        let message = err.to_string();
        assert!(
            message.contains("server.verbose_errors"),
            "the error must name the setting to change: {message}"
        );
    }
}
