//! TLS/HTTPS coverage (T1).
//!
//! Before this file nothing in the repo matched `tls` case-insensitively —
//! not the cert/key validation errors, not `load_rustls_config`, not a real
//! handshake, and not the TLS serve loop whose graceful-shutdown path was
//! broken until 1.0.0. Covered here:
//!   1. config validation rejects missing/blank cert and key paths
//!   2. `load_rustls_config` succeeds on a real pair and fails on garbage
//!   3. one HTTPS round-trip against a bound server, chain-validated
//!   4. a drain over TLS, mirroring `drain_test.rs` (plain HTTP)
//!
//! The certificate is generated in-process with `rcgen`, so these tests
//! always run — no external `openssl` dependency and no silent skip path.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::common;

// ============================================================
// Fixtures
// ============================================================

/// Self-cleaning scratch directory; `tempfile` is not in the dependency tree.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "orion_tls_test_{}_{}_{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Generate a self-signed P-256 leaf valid for `localhost` and `127.0.0.1`,
/// written as PEM files under `dir`. Returns `(cert_path, key_path)`.
fn self_signed_pair(dir: &Path) -> (String, String) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("generate self-signed certificate");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).expect("write cert");
    std::fs::write(&key, certified.signing_key.serialize_pem()).expect("write key");
    (
        cert.to_string_lossy().into_owned(),
        key.to_string_lossy().into_owned(),
    )
}

/// Write a config file enabling TLS with the given paths and load it through
/// the real `load_config` path (which runs `ServerConfig::validate`).
fn load_tls_config(dir: &Path, cert_path: &str, key_path: &str) -> Result<(), String> {
    let toml = format!(
        "[server]\nport = 8080\n\n[server.tls]\nenabled = true\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert_path.replace('\\', "\\\\"),
        key_path.replace('\\', "\\\\"),
    );
    let path = dir.join("orion.toml");
    std::fs::write(&path, toml).expect("write config");
    orion::config::load_config(Some(&path.to_string_lossy()))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ============================================================
// 1. Config validation
// ============================================================

#[test]
fn test_tls_config_rejects_missing_cert_file() {
    let dir = ScratchDir::new("cfg_missing_cert");
    let key = dir.path().join("key.pem");
    std::fs::write(&key, "not a real key").expect("write key");

    let err = load_tls_config(
        dir.path(),
        &dir.path().join("nope.pem").to_string_lossy(),
        &key.to_string_lossy(),
    )
    .expect_err("missing cert must fail validation");
    assert!(
        err.contains("certificate file not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_tls_config_rejects_missing_key_file() {
    let dir = ScratchDir::new("cfg_missing_key");
    let cert = dir.path().join("cert.pem");
    std::fs::write(&cert, "not a real cert").expect("write cert");

    let err = load_tls_config(
        dir.path(),
        &cert.to_string_lossy(),
        &dir.path().join("nope.pem").to_string_lossy(),
    )
    .expect_err("missing key must fail validation");
    assert!(
        err.contains("private key file not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_tls_config_rejects_blank_paths() {
    let dir = ScratchDir::new("cfg_blank");
    let err = load_tls_config(dir.path(), "", "").expect_err("blank cert_path must fail");
    assert!(err.contains("cert_path"), "unexpected error: {err}");
}

#[test]
fn test_tls_config_accepts_existing_pair() {
    let dir = ScratchDir::new("cfg_ok");
    let (cert, key) = self_signed_pair(dir.path());
    load_tls_config(dir.path(), &cert, &key).expect("a real pair must validate");
}

// ============================================================
// 2. load_rustls_config
// ============================================================

#[tokio::test]
async fn test_load_rustls_config_succeeds_on_real_pair() {
    let dir = ScratchDir::new("load_ok");
    let (cert, key) = self_signed_pair(dir.path());
    orion::server::tls::load_rustls_config(&cert, &key)
        .await
        .expect("valid PEM pair must load");
}

#[tokio::test]
async fn test_load_rustls_config_rejects_unreadable_files() {
    let dir = ScratchDir::new("load_missing");
    let missing = dir.path().join("absent.pem").to_string_lossy().into_owned();

    let err = orion::server::tls::load_rustls_config(&missing, &missing)
        .await
        .expect_err("nonexistent files must not load");
    let msg = err.to_string();
    assert!(msg.contains("Failed to initialize TLS"), "got: {msg}");
    assert!(
        msg.contains(&missing),
        "the error must name the offending path, got: {msg}"
    );
}

#[tokio::test]
async fn test_load_rustls_config_rejects_non_pem_content() {
    let dir = ScratchDir::new("load_garbage");
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, b"-----BEGIN CERTIFICATE-----\nnot base64\n").expect("write cert");
    std::fs::write(&key, b"definitely not a key").expect("write key");

    orion::server::tls::load_rustls_config(&cert.to_string_lossy(), &key.to_string_lossy())
        .await
        .expect_err("garbage PEM must not load");
}

// ============================================================
// 3. HTTPS round-trip
// ============================================================

/// Serve the real router through the REAL `orion::server::serve::serve_tls`
/// (the exact code `main.rs` dispatches to) on an ephemeral port — port 0 in
/// the config, actual address read back via `handle.listening()`. Returns
/// the bound address, the axum-server handle, the shutdown trigger for the
/// drain sequence, the serve task, and the readiness flag.
async fn spawn_real_serve_tls(
    cert: &str,
    key: &str,
) -> (
    SocketAddr,
    axum_server::Handle<SocketAddr>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), orion::errors::OrionError>>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut config = orion::config::AppConfig::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.port = 0;
    config.server.tls.enabled = true;
    config.server.tls.cert_path = cert.to_string();
    config.server.tls.key_path = key.to_string();
    config.server.shutdown_drain_secs = 2;
    config.server.shutdown_force_timeout_secs = 5;

    let state = common::test_state_with_config(config).await;
    let ready = state.ready.clone();
    let cfg = state.config.clone();
    let router = orion::server::build_router(state);
    let handle = axum_server::Handle::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(orion::server::serve::serve_tls(
        cfg,
        ready.clone(),
        router,
        handle.clone(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    let addr = handle.listening().await.expect("TLS server must bind");
    (addr, handle, shutdown_tx, task, ready)
}

/// A client that validates the chain against our own self-signed CA — a
/// handshake that only succeeds if the server really presents that cert.
fn https_client(cert_path: &str) -> reqwest::Client {
    let pem = std::fs::read(cert_path).expect("read cert");
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&pem).expect("parse cert"))
        // Fresh connection per request: reuse would mask whether the listener
        // still accepts during the drain test.
        .pool_max_idle_per_host(0)
        .build()
        .expect("client")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_https_round_trip() {
    let dir = ScratchDir::new("roundtrip");
    let (cert, key) = self_signed_pair(dir.path());

    let (addr, handle, _shutdown_tx, task, _ready) = spawn_real_serve_tls(&cert, &key).await;

    let client = https_client(&cert);
    let resp = client
        .get(format!("https://localhost:{}/healthz", addr.port()))
        .send()
        .await
        .expect("HTTPS request must succeed over the self-signed chain");
    assert_eq!(resp.status(), 200);

    // The same URL over plain HTTP must not be served as HTTP.
    let plain = reqwest::Client::builder()
        .build()
        .expect("client")
        .get(format!("http://127.0.0.1:{}/healthz", addr.port()))
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    assert!(plain.is_err(), "a TLS listener must not answer plain HTTP");

    // Immediate stop — skips the drain sequence, which section 4 covers.
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// ============================================================
// 4. Drain over TLS
// ============================================================

/// The plain-HTTP equivalent lives in `drain_test.rs`. The 1.0.0 CHANGELOG
/// records that the TLS shutdown path was broken until this release, so the
/// same sequence is asserted here: readiness withdrawn first, connections
/// still accepted through the grace window, then accept stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tls_drain_withdraws_readiness_before_stopping_accept() {
    let dir = ScratchDir::new("drain");
    let (cert, key) = self_signed_pair(dir.path());

    let (addr, _handle, shutdown_tx, task, ready) = spawn_real_serve_tls(&cert, &key).await;
    assert!(ready.load(Ordering::Acquire));
    let base = format!("https://localhost:{}", addr.port());
    let client = https_client(&cert);

    // 1. Healthy before the signal.
    let resp = client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("readyz before signal");
    assert_eq!(resp.status(), 200);

    // 2. Signal. The drain sequence (withdraw readiness, keep accepting
    // through the grace window, then graceful_shutdown) runs INSIDE the real
    // serve_tls — nothing is re-implemented here.
    shutdown_tx.send(()).expect("send shutdown");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("readyz reachable during grace");
    assert_eq!(resp.status(), 503, "readyz must flip to 503 during drain");

    // ...and a brand-new TLS connection is still accepted during the window.
    let resp = client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("new TLS connection during grace must be accepted");
    assert_eq!(resp.status(), 200);

    // 3. After the grace window the server stops accepting and exits.
    let result = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("TLS server must stop within the grace window + slack")
        .expect("join");
    result.expect("serve returns cleanly");

    let refused = client
        .get(format!("{base}/healthz"))
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "requests must fail once the TLS listener has drained"
    );
}
