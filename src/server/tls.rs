//! TLS certificate loading and configuration for HTTPS support.

use axum_server::tls_rustls::RustlsConfig;

use crate::errors::OrionError;

/// rustls 0.23 only auto-selects a crypto provider when *exactly one* of its
/// `aws-lc-rs` / `ring` features is enabled. Orion's dependency graph enables
/// both — axum-server and reqwest pull in `aws-lc-rs`, mongodb and sqlx pull in
/// `ring` — so without an explicit install every rustls entry point panics with
/// "Could not automatically determine the process-level CryptoProvider" and
/// `server.tls.enabled = true` takes the process down at boot.
///
/// `aws-lc-rs` is the provider `axum-server`'s own `tls-rustls` feature selects.
/// An `Err` from `install_default` means something else installed one first,
/// which is equally fine — the only unacceptable state is no provider at all.
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Load a `RustlsConfig` from PEM certificate chain and private key files.
pub async fn load_rustls_config(
    cert_path: &str,
    key_path: &str,
) -> Result<RustlsConfig, OrionError> {
    ensure_crypto_provider();
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
    use super::*;

    #[test]
    fn installs_a_process_level_crypto_provider() {
        ensure_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "rustls cannot pick a provider from features alone in this tree"
        );
    }
}
