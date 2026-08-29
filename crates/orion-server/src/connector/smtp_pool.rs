//! Cached SMTP connections for the `send_email` function (#262).
//!
//! One pool per connector, LRU-capped like the SQL/Mongo/Redis caches and
//! evicted alongside them when a connector changes (`evict_connector_pools`).
//! A burst of sends reuses live connections instead of paying a TCP and TLS
//! handshake per email.
//!
//! The pool is Orion's own because mail-send has none: `SmtpClient` is a
//! single live connection taking `&mut self`, so it cannot be shared the way
//! lettre's internally-pooled transport could be cloned.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mail_send::{Credentials, SmtpClient, SmtpClientBuilder};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::client::TlsStream;

use super::lru_cache::LruCache;
use crate::connector::config::{SmtpAuth, SmtpConnectorConfig, SmtpTls};
use crate::errors::OrionError;

/// How many live connections one connector keeps parked. Beyond this a
/// returned connection is dropped rather than queued — a relay that is being
/// hammered by more than this many concurrent sends is better served by
/// opening a connection than by unbounded parking.
const MAX_IDLE_PER_CONNECTOR: usize = 4;

/// How long a parked connection stays eligible for reuse. SMTP servers drop
/// idle sessions unannounced (RFC 5321 §4.5.3.2 puts the server-side minimum
/// at 5 minutes, and relays routinely undercut it), so anything older is
/// closed unread rather than gambled on.
const MAX_IDLE_AGE: Duration = Duration::from_secs(60);

/// TLS and plaintext connections have different concrete types —
/// `SmtpClient<TlsStream<TcpStream>>` against `SmtpClient<TcpStream>` — so one
/// pool cannot hold both without a common stream. `SmtpClient`'s fields are
/// public, so the connection is re-wrapped once after connecting; the
/// alternative, a boxed trait object, would pay dynamic dispatch on every
/// read of every line.
pub enum SmtpStream {
    Tls(Box<TlsStream<TcpStream>>),
    Plain(TcpStream),
}

/// Both variants are `Unpin`, so the pin projection is a plain re-borrow.
macro_rules! project {
    ($self:ident) => {
        match $self.get_mut() {
            SmtpStream::Tls(s) => std::pin::Pin::new(&mut **s) as std::pin::Pin<&mut dyn Stream>,
            SmtpStream::Plain(s) => std::pin::Pin::new(s) as std::pin::Pin<&mut dyn Stream>,
        }
    };
}

/// The two capabilities `SmtpClient` requires, in one object-safe trait so the
/// projection above has a single return type.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

impl tokio::io::AsyncRead for SmtpStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        project!(self).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for SmtpStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        project!(self).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        project!(self).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        project!(self).poll_shutdown(cx)
    }
}

/// A connection checked out of (and returned to) a [`SmtpPool`].
pub type PooledClient = SmtpClient<SmtpStream>;

struct Idle {
    client: PooledClient,
    parked_at: Instant,
}

/// The live connections for one connector, plus the recipe for opening more.
pub struct SmtpPool {
    builder: SmtpClientBuilder<String>,
    tls: SmtpTls,
    idle: Mutex<Vec<Idle>>,
}

impl SmtpPool {
    /// Take a connection: a parked one that still answers, else a fresh one.
    ///
    /// Reuse is verified with `RSET` rather than assumed. That costs one
    /// round trip against a handshake's several, and it is the only check
    /// that is *safe* to retry — it happens before `MAIL FROM`, so nothing
    /// has been committed. Once `DATA` is in flight the no-retry rule in
    /// `send_email` applies and a dead connection is an error, never a
    /// second attempt.
    pub async fn checkout(&self) -> Result<PooledClient, mail_send::Error> {
        loop {
            let Some(idle) = self.idle.lock().await.pop() else {
                break;
            };
            if idle.parked_at.elapsed() >= MAX_IDLE_AGE {
                continue; // too old to trust; dropping closes it
            }
            let mut client = idle.client;
            // RSET doubles as a liveness probe and a state reset, so a
            // connection abandoned mid-transaction cannot poison the next send.
            if client.rset().await.is_ok() {
                return Ok(client);
            }
        }
        self.connect().await
    }

    /// Park a connection for reuse. Callers return only connections that
    /// completed a send cleanly — anything that errored is dropped, since its
    /// protocol state is unknown.
    pub async fn checkin(&self, client: PooledClient) {
        let mut idle = self.idle.lock().await;
        if idle.len() < MAX_IDLE_PER_CONNECTOR {
            idle.push(Idle {
                client,
                parked_at: Instant::now(),
            });
        }
        // Otherwise drop it: the TCP close is the QUIT we cannot await here.
    }

    async fn connect(&self) -> Result<PooledClient, mail_send::Error> {
        // `connect` covers both TLS shapes: implicit when the builder says so,
        // otherwise STARTTLS — and it fails with `MissingStartTls` when the
        // server does not offer it, which is the "required" semantic Orion
        // wants rather than a silent downgrade.
        Ok(match self.tls {
            SmtpTls::None => {
                let client = self.builder.connect_plain().await?;
                SmtpClient {
                    stream: SmtpStream::Plain(client.stream),
                    timeout: client.timeout,
                }
            }
            SmtpTls::Starttls | SmtpTls::Implicit => {
                let client = self.builder.connect().await?;
                SmtpClient {
                    stream: SmtpStream::Tls(Box::new(client.stream)),
                    timeout: client.timeout,
                }
            }
        })
    }
}

pub struct SmtpPoolCache {
    cache: LruCache<Arc<SmtpPool>>,
}

impl SmtpPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            // Dropping a pool closes its parked connections; nothing needs
            // awaiting, so no evict handler.
            cache: LruCache::new(max_entries, "smtp_pool"),
        }
    }

    /// Resolve (and cache) the pool for `connector_name`.
    ///
    /// The S6 private-address check runs here, on the pool-open path, like
    /// the Mongo cache: storing a connector must not depend on DNS, but
    /// opening a connection to it may.
    pub async fn get_pool(
        &self,
        connector_name: &str,
        config: &SmtpConnectorConfig,
    ) -> Result<Arc<SmtpPool>, OrionError> {
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
                build_pool(&name, &config)
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

/// Build the pool for one connector config.
///
/// TLS validates against the platform trust store (mail-send builds its
/// connector on `rustls-platform-verifier`); there is deliberately no
/// skip-verification path — a private-CA relay is served by installing the CA
/// at the OS level, not by a downgrade knob.
fn build_pool(
    connector_name: &str,
    config: &SmtpConnectorConfig,
) -> Result<Arc<SmtpPool>, OrionError> {
    // Before `SmtpClientBuilder::new`, which builds its rustls connector
    // eagerly — even for `SmtpTls::None` — and panics if no process-level
    // crypto provider has been chosen. See the helper for why Orion's graph
    // cannot auto-select one.
    crate::crypto::ensure_provider();

    let mut builder = SmtpClientBuilder::new(config.host.clone(), config.port)
        .map_err(|e| OrionError::validation(format!("SMTP connector '{connector_name}': {e}")))?
        .implicit_tls(matches!(config.tls, SmtpTls::Implicit))
        .timeout(Duration::from_millis(config.timeout_ms));

    if let SmtpAuth::Basic { username, password } = &config.auth {
        builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
    }

    Ok(Arc::new(SmtpPool {
        builder,
        tls: config.tls,
        idle: Mutex::new(Vec::new()),
    }))
}
