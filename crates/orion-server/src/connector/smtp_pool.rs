//! Cached SMTP transports for the `send_email` function (#262).
//!
//! One pooled [`AsyncSmtpTransport`] per connector, LRU-capped like the
//! SQL/Mongo/Redis caches and evicted alongside them when a connector
//! changes (`evict_connector_pools`). lettre's own connection pool sits
//! inside the transport, so a burst of sends reuses live connections
//! instead of paying a TLS handshake per email.

use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, Tokio1Executor};

use super::lru_cache::LruCache;
use crate::connector::config::{SmtpAuth, SmtpConnectorConfig, SmtpTls};
use crate::errors::OrionError;

pub struct SmtpPoolCache {
    cache: LruCache<AsyncSmtpTransport<Tokio1Executor>>,
}

impl SmtpPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            // Dropping a transport closes its pooled connections; nothing
            // needs awaiting, so no evict handler.
            cache: LruCache::new(max_entries, "smtp_pool"),
        }
    }

    /// Resolve (and cache) the transport for `connector_name`.
    ///
    /// The S6 private-address check runs here, on the pool-open path, like
    /// the Mongo cache: storing a connector must not depend on DNS, but
    /// opening a connection to it may.
    pub async fn get_transport(
        &self,
        connector_name: &str,
        config: &SmtpConnectorConfig,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, OrionError> {
        let config = config.clone();
        let name = connector_name.to_string();
        self.cache
            .get_or_create(connector_name, || async move {
                if !config.allow_private_urls
                    && let Err(msg) =
                        crate::validation::validate_hostport_not_private(&config.host, config.port)
                            .await
                {
                    return Err(OrionError::validation(format!(
                        "SMTP connector '{name}': {msg} (set allow_private_urls for an \
                         internal relay)"
                    )));
                }
                build_transport(&name, &config)
            })
            .await
    }

    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }

    pub async fn evict_all(&self) {
        self.cache.evict_all().await;
    }
}

impl Default for SmtpPoolCache {
    fn default() -> Self {
        Self::new(64)
    }
}

/// Build the pooled transport for one connector config.
///
/// TLS validates against the platform trust store (rustls-native-certs);
/// there is deliberately no skip-verification path — a private-CA relay is
/// served by installing the CA at the OS level, not by a downgrade knob.
fn build_transport(
    connector_name: &str,
    config: &SmtpConnectorConfig,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, OrionError> {
    let tls_error = |e: lettre::transport::smtp::Error| OrionError::Internal {
        context: format!("SMTP connector '{connector_name}': TLS setup failed"),
        source: Some(Box::new(e)),
    };

    let mut builder =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host).port(config.port);

    builder = match config.tls {
        SmtpTls::Starttls => {
            let params = TlsParameters::new(config.host.clone()).map_err(tls_error)?;
            builder.tls(Tls::Required(params))
        }
        SmtpTls::Implicit => {
            let params = TlsParameters::new(config.host.clone()).map_err(tls_error)?;
            builder.tls(Tls::Wrapper(params))
        }
        // Dev/localhost relays; validation warns loudly on this mode.
        SmtpTls::None => builder.tls(Tls::None),
    };

    if let SmtpAuth::Basic { username, password } = &config.auth {
        builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
    }

    builder = builder.timeout(Some(std::time::Duration::from_millis(config.timeout_ms)));

    Ok(builder.build())
}
