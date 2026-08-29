//! TLS certificate loading and configuration for HTTPS support.

use axum_server::tls_rustls::RustlsConfig;

use crate::errors::OrionError;
/// Load a `RustlsConfig` from PEM certificate chain and private key files.
pub async fn load_rustls_config(
    cert_path: &str,
    key_path: &str,
) -> Result<RustlsConfig, OrionError> {
    crate::crypto::ensure_provider();
    RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| OrionError::Internal {
            context: format!(
                "Failed to initialize TLS from cert='{cert_path}' key='{key_path}'. \
                 Verify that both are valid PEM-encoded files."
            ),
            source: Some(Box::new(e)),
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_a_process_level_crypto_provider() {
        crate::crypto::ensure_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "rustls cannot pick a provider from features alone in this tree"
        );
    }
}
