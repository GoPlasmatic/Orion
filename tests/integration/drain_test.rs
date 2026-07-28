//! Rolling-deploy drain drill (multi-instance-ha A9).
//!
//! Boots the real router through the REAL `serve_plain_http` (the exact code
//! `main.rs` dispatches to — including `create_tcp_listener`'s socket2 setup
//! and the force-timeout arm) with the shutdown future driven by a channel
//! instead of SIGTERM, and asserts the sequence a load balancer depends on:
//!   1. before signal: /readyz is 200
//!   2. on signal: /readyz flips to 503 immediately, but NEW connections are
//!      still accepted and served during the grace window
//!   3. after the grace window: the server stops accepting and the serve
//!      future completes (in-flight requests finished)

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_withdraws_readiness_before_stopping_accept() {
    let mut config = orion::config::AppConfig::default();
    config.server.shutdown_drain_secs = 2;
    config.server.shutdown_force_timeout_secs = 5;
    let state = common::test_state_with_config(config).await;
    let ready = state.ready.clone();
    assert!(ready.load(Ordering::Acquire));
    let cfg = state.config.clone();
    let router = orion::server::build_router(state);

    let listener = orion::server::serve::create_tcp_listener("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let base = format!("http://{addr}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(orion::server::serve::serve_plain_http(
        listener,
        cfg,
        ready.clone(),
        router,
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let client = reqwest::Client::builder()
        // Fresh TCP connection per request — connection reuse would mask
        // whether the listener still accepts.
        .pool_max_idle_per_host(0)
        .build()
        .expect("client");

    // 1. Healthy before the signal.
    let resp = client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("readyz before signal");
    assert_eq!(resp.status(), 200);

    // 2. Signal: readiness withdrawn immediately...
    shutdown_tx.send(()).expect("send shutdown");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("readyz reachable during grace");
    assert_eq!(resp.status(), 503, "readyz must flip to 503 during drain");

    // ...but new connections still get served during the grace window.
    let resp = client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("new connection during grace must be accepted");
    assert_eq!(resp.status(), 200);

    // 3. After the grace window the server stops accepting and exits.
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server must stop within the grace window + slack")
        .expect("join");
    result.expect("serve returns cleanly");

    let connect_after = tokio::net::TcpStream::connect(addr).await;
    if let Ok(stream) = connect_after {
        // Some platforms accept the TCP handshake into a dead socket's
        // backlog; a read must then fail/EOF immediately.
        let mut buf = [0u8; 1];
        use tokio::io::AsyncReadExt;
        let mut stream = stream;
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("read should resolve")
            .unwrap_or(0);
        assert_eq!(n, 0, "socket must be closed after drain");
    }
}
