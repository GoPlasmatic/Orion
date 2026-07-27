use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
        }
    }
}

impl ServerConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(u64::from(self.port), "server.port")?;
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
#[serde(default)]
pub struct CompressionConfig {
    pub enabled: bool,
}

/// TLS configuration for HTTPS support.
/// When `enabled` is false (default), the server runs plain HTTP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Enable TLS. Requires `cert_path` and `key_path` to be set.
    pub enabled: bool,
    /// Path to the PEM-encoded certificate chain file.
    pub cert_path: String,
    /// Path to the PEM-encoded private key file.
    pub key_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
