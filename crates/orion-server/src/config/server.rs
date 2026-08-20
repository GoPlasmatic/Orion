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
    /// forever (pre-1.0 behaviour of the plain-HTTP path).
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
    /// honoured — see `ServerConfig::validate`.
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

    /// Additional path prefixes the data plane is served at, on top of the
    /// always-present `/api/v1/data`.
    ///
    /// For fronting deployed clients that call paths at the server root
    /// (`/zoom/meetings/user`, `/Legacy-App/api/public/...`) — today every such
    /// deployment needs a reverse proxy whose only job is to prepend the
    /// prefix. `route_pattern`s are already multi-segment and unrestricted, so
    /// only the mount point was missing.
    ///
    /// **Additive, never a movable prefix.** `/api/v1/data` stays mounted, so
    /// every existing client, doc example, test and `orion-client` path keeps
    /// working — a moved prefix would break `orion-cli` and the MCP data tool
    /// against that server with no discovery mechanism.
    ///
    /// Prefer **named** mounts. The literal `"/"` is accepted as an explicit
    /// escape hatch, but it re-opens an upgrade hazard: a future platform
    /// route at, say, `/version` would silently shadow a channel already
    /// serving `route_pattern = "/version"` — a wrong answer, not an error.
    /// A named mount claims a first-segment namespace instead, which lets
    /// Orion hold the invariant that platform routes are single-segment at
    /// root or under `/api`.
    pub data_mounts: Vec<String>,
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
            data_mounts: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// Structural checks on `data_mounts`.
    ///
    /// Refused rather than sanitized, matching the `verbose_errors` posture
    /// above: a mount is what the data plane answers on, so a malformed one is
    /// a deployment that serves the wrong surface, not a value to guess at.
    fn validate_data_mounts(&self) -> Result<(), OrionError> {
        let err = |message: String| OrionError::Config { message };
        let mut seen: Vec<&str> = Vec::with_capacity(self.data_mounts.len());

        for mount in &self.data_mounts {
            // The explicit root escape hatch. Everything below assumes a named
            // mount, so it is handled first.
            if mount == "/" {
                if seen.contains(&mount.as_str()) {
                    return Err(err("server.data_mounts contains \"/\" twice".to_string()));
                }
                seen.push(mount);
                continue;
            }
            if !mount.starts_with('/') {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' must start with '/'"
                )));
            }
            if mount.ends_with('/') {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' must not end with '/'"
                )));
            }
            if mount.contains("//") {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' has an empty path segment"
                )));
            }
            // A mount is static — parameters belong in a channel's
            // `route_pattern`, which is matched underneath it.
            if let Some(bad) = mount
                .chars()
                .find(|c| c.is_whitespace() || *c == '%' || *c == '{' || *c == '}')
            {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' contains '{bad}' — a mount is a \
                     static prefix; path parameters belong in a channel's route_pattern"
                )));
            }
            // Reserved: the same rule the channel-activation gate applies, so
            // the two cannot drift. (A mount that *contains* a platform route
            // cannot occur: every `PLATFORM_ROUTES` entry is single-segment,
            // so a strict prefix of one would have to be `""`, which fails the
            // leading-`/` check above.)
            if let Some(reserved) = crate::server::routes::shadowed_platform_route(mount) {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' collides with the platform \
                     route '{reserved}' — mounting the data plane there would shadow \
                     it or be shadowed by it"
                )));
            }
            if seen.contains(&mount.as_str()) {
                return Err(err(format!(
                    "server.data_mounts entry '{mount}' is listed twice"
                )));
            }
            // Two catch-alls under one branch is a matchit insertion conflict,
            // which panics at router construction. Refuse it here with a
            // readable message instead of crashing at boot.
            if let Some(other) = seen.iter().find(|s| {
                **s != "/"
                    && (mount.starts_with(&format!("{s}/")) || s.starts_with(&format!("{mount}/")))
            }) {
                return Err(err(format!(
                    "server.data_mounts entries '{other}' and '{mount}' nest — two \
                     catch-alls under one branch is a router conflict; keep mounts \
                     disjoint"
                )));
            }
            seen.push(mount);
        }

        if seen.contains(&"/") {
            tracing::warn!(
                "server.data_mounts includes \"/\": the data plane answers every \
                 unmatched path, so an unknown URL becomes a channel lookup rather \
                 than a 404 — and a future platform route could shadow a channel \
                 serving the same path. Prefer named mounts."
            );
        }
        Ok(())
    }

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
        self.validate_data_mounts()?;
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

    fn with_mounts(mounts: &[&str]) -> ServerConfig {
        ServerConfig {
            data_mounts: mounts.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn named_mounts_are_accepted() {
        assert!(
            with_mounts(&["/zoom", "/Legacy-App"])
                .validate(false)
                .is_ok()
        );
        assert!(with_mounts(&["/a/b/c"]).validate(false).is_ok());
        // The default — and the explicit root escape hatch.
        assert!(with_mounts(&[]).validate(false).is_ok());
        assert!(with_mounts(&["/"]).validate(false).is_ok());
    }

    /// A mount that claims a platform route would either shadow it or be
    /// shadowed by it; either way the deployment serves the wrong surface.
    #[test]
    fn a_mount_cannot_claim_a_platform_route() {
        for mount in [
            "/api",
            "/api/v1",
            "/api/v1/data",
            "/api/v1/admin",
            "/health",
            "/healthz",
            "/readyz",
            "/metrics",
            "/docs",
            "/docs/assets",
        ] {
            let err = with_mounts(&[mount])
                .validate(false)
                .expect_err("must be refused")
                .to_string();
            assert!(err.contains("platform route"), "{mount}: {err}");
        }
    }

    #[test]
    fn malformed_mounts_are_refused_not_sanitized() {
        for (mount, needle) in [
            ("zoom", "must start with"),
            ("/zoom/", "must not end with"),
            ("/zoom//x", "empty path segment"),
            ("/zo om", "static prefix"),
            ("/zoom/{id}", "static prefix"),
            ("/zo%6fm", "static prefix"),
        ] {
            let err = with_mounts(&[mount])
                .validate(false)
                .expect_err("must be refused")
                .to_string();
            assert!(err.contains(needle), "{mount}: {err}");
        }
    }

    /// Two catch-alls under one branch is a matchit insertion conflict, which
    /// panics at router construction — refused here with a readable message
    /// rather than crashing at boot.
    #[test]
    fn nested_or_duplicate_mounts_are_refused() {
        let err = with_mounts(&["/zoom", "/zoom/deep"])
            .validate(false)
            .expect_err("nesting must be refused")
            .to_string();
        assert!(err.contains("nest"), "{err}");

        let err = with_mounts(&["/zoom", "/zoom"])
            .validate(false)
            .expect_err("a duplicate must be refused")
            .to_string();
        assert!(err.contains("twice"), "{err}");

        assert!(
            with_mounts(&["/", "/"]).validate(false).is_err(),
            "a duplicated root mount is still a duplicate"
        );
        // A named mount alongside "/" is legal: the root catch-all and a
        // deeper one do not conflict in matchit.
        assert!(with_mounts(&["/", "/zoom"]).validate(false).is_ok());
    }

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
