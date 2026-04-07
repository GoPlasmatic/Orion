//! TLS certificate loading and configuration for HTTPS support.

use axum_server::tls_rustls::RustlsConfig;

use crate::errors::OrionError;

/// Load a `RustlsConfig` from PEM certificate chain and private key files.
pub async fn load_rustls_config(
    cert_path: &str,
    key_path: &str,
) -> Result<RustlsConfig, OrionError> {
    RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: format!(
                "Failed to initialize TLS from cert='{}' key='{}'. \
                 Verify that both are valid PEM-encoded files.",
                cert_path, key_path
            ),
            source: Box::new(e),
        })
}
