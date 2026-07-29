//! The serving loops: TCP listener construction, plain-HTTP and TLS serving
//! with the shared drain sequence. Lives in the lib (not `main.rs`) so the
//! integration tests exercise the real accept/drain paths instead of
//! re-implementing them.
//!
//! Seams for tests: `serve_tls` takes an externally created
//! [`axum_server::Handle`] (its `listening()` reports the bound address, so
//! `"127.0.0.1:0"` works), and `serve_plain_http` takes a pre-bound listener
//! (callers compose [`create_tcp_listener`] + `local_addr()`). Both take the
//! shutdown future as a parameter — `main.rs` passes
//! [`shutdown_signal`](super::shutdown_signal), tests pass a oneshot.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::config::AppConfig;
use crate::errors::OrionError;

/// Create a bound TCP listener with `TCP_NODELAY` enabled (avoids Nagle's
/// 40 ms latency on small responses) and `SO_REUSEADDR` set.
pub fn create_tcp_listener(addr: &str) -> Result<tokio::net::TcpListener, OrionError> {
    let socket_addr = addr
        .parse::<std::net::SocketAddr>()
        .map_err(|e| OrionError::Internal(format!("Invalid address '{addr}': {e}")))?;
    let domain = if socket_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let map_err = |stage: &str, e: std::io::Error| OrionError::InternalSource {
        context: format!("Failed to {stage} for {addr}"),
        source: Box::new(e),
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
        .map_err(|e| map_err("create socket", e))?;
    socket.set_tcp_nodelay(true).ok();
    socket.set_reuse_address(true).ok();
    socket
        .bind(&socket_addr.into())
        .map_err(|e| map_err("bind", e))?;
    socket.listen(1024).map_err(|e| map_err("listen", e))?;
    socket.set_nonblocking(true).ok();
    tokio::net::TcpListener::from_std(socket.into())
        .map_err(|e| map_err("create async listener", e))
}

/// Bind a TLS (HTTPS) listener via axum-server and serve `router` with
/// graceful shutdown — same drain sequence as the plain-HTTP path.
///
/// The bind address comes from `config.server.host`/`port`; with port 0 the
/// actual address is available via `handle.listening().await`.
pub async fn serve_tls(
    config: Arc<AppConfig>,
    ready: Arc<AtomicBool>,
    router: axum::Router,
    handle: axum_server::Handle<std::net::SocketAddr>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), OrionError> {
    let rustls_config =
        super::tls::load_rustls_config(&config.server.tls.cert_path, &config.server.tls.key_path)
            .await?;

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let bind_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| OrionError::Internal(format!("Invalid address '{addr}': {e}")))?;
    let shutdown_handle = handle.clone();
    let drain_secs = config.server.shutdown_drain_secs;
    let force_timeout_secs = config.server.shutdown_force_timeout_secs;
    let ready_for_drain = ready.clone();
    tokio::spawn(async move {
        // Withdraw readiness, keep accepting through the LB grace window,
        // THEN stop accepting — same sequence as the plain-HTTP path.
        super::drain::drain_gate(
            shutdown,
            ready_for_drain,
            std::time::Duration::from_secs(drain_secs),
        )
        .await;
        let force =
            (force_timeout_secs > 0).then(|| std::time::Duration::from_secs(force_timeout_secs));
        shutdown_handle.graceful_shutdown(force);
    });

    tracing::info!(
        address = %addr,
        storage = %config.storage.url,
        tls = true,
        "Orion is ready (HTTPS)"
    );

    axum_server::bind_rustls(bind_addr, rustls_config)
        .handle(handle)
        .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .map_err(|e| OrionError::InternalSource {
            context: format!("HTTPS server error on {addr}"),
            source: Box::new(e),
        })
}

/// Serve the dedicated metrics listener (`metrics.bind_addr`, O12) over a
/// pre-bound plain-HTTP listener.
///
/// Joins the existing shutdown path rather than inventing one: it keeps
/// accepting for the same `server.shutdown_drain_secs` grace the main listener
/// observes, so the scrape that lands while the load balancer is still
/// draining this node still succeeds, and it stops accepting at the same
/// moment. It does **not** touch the readiness flag — `/readyz` belongs to the
/// main listener, and two writers flipping one flag would make the drain
/// sequence depend on task scheduling.
///
/// Plain HTTP by design; `server.tls` governs the main listener only.
pub async fn serve_metrics(
    listener: tokio::net::TcpListener,
    config: Arc<AppConfig>,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), OrionError> {
    let drain = std::time::Duration::from_secs(config.server.shutdown_drain_secs);
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    tracing::info!(
        address = %addr,
        "Metrics listener ready (GET /metrics, unauthenticated)"
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.await;
            tokio::time::sleep(drain).await;
        })
        .await
        .map_err(|e| OrionError::InternalSource {
            context: format!("Metrics server error on {addr}"),
            source: Box::new(e),
        })
}

/// Serve `router` over a pre-bound plain (non-TLS) HTTP listener with
/// graceful shutdown.
pub async fn serve_plain_http(
    listener: tokio::net::TcpListener,
    config: Arc<AppConfig>,
    ready: Arc<AtomicBool>,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), OrionError> {
    let drain_secs = config.server.shutdown_drain_secs;
    let force_timeout_secs = config.server.shutdown_force_timeout_secs;
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    tracing::info!(
        address = %addr,
        storage = %config.storage.url,
        tcp_nodelay = true,
        "Orion is ready"
    );
    // `drained` fires when the gate resolves (accept stopped); the force
    // timeout counts from that point, bounding the in-flight wait.
    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel::<()>();
    let gate = async move {
        super::drain::drain_gate(shutdown, ready, std::time::Duration::from_secs(drain_secs)).await;
        let _ = drained_tx.send(());
    };
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(gate);
    let map_err = |e: std::io::Error| OrionError::InternalSource {
        context: format!("HTTP server error on {addr}"),
        source: Box::new(e),
    };
    if force_timeout_secs == 0 {
        serve.await.map_err(map_err)?;
    } else {
        tokio::select! {
            result = serve => result.map_err(map_err)?,
            _ = async {
                // Wait for the gate, then bound the in-flight drain. If the
                // server finishes first the sender is dropped and this arm
                // pends forever — the select resolves through `serve`.
                match drained_rx.await {
                    Ok(()) => {
                        tokio::time::sleep(std::time::Duration::from_secs(force_timeout_secs)).await;
                        tracing::warn!(
                            force_timeout_secs,
                            "Shutdown force timeout elapsed; aborting remaining in-flight connections"
                        );
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    Ok(())
}
