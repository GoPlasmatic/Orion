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
//! The certificate is generated at test time with the `openssl` CLI. When
//! `openssl` is unavailable the TLS tests report a skip rather than failing —
//! every CI runner and dev image used by this project ships it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
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

fn openssl_available() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate a self-signed P-256 leaf valid for `localhost` and `127.0.0.1`.
/// Returns `None` when `openssl` is not on PATH so callers can skip.
fn self_signed_pair(dir: &Path) -> Option<(String, String)> {
    if !openssl_available() {
        return None;
    }
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "365",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            // webpki rejects a trust anchor reused as the leaf unless the leaf
            // carries serverAuth (`EkuError` otherwise).
            "-addext",
            "extendedKeyUsage=serverAuth",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .ok()?;
    if !status.status.success() {
        eprintln!(
            "openssl failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        return None;
    }
    Some((
        cert.to_string_lossy().into_owned(),
        key.to_string_lossy().into_owned(),
    ))
}

fn skip(test: &str) {
    eprintln!("SKIP {test}: `openssl` is not available to generate a test certificate");
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
    let Some((cert, key)) = self_signed_pair(dir.path()) else {
        return skip("test_tls_config_accepts_existing_pair");
    };
    load_tls_config(dir.path(), &cert, &key).expect("a real pair must validate");
}

// ============================================================
// 2. load_rustls_config
// ============================================================

#[tokio::test]
async fn test_load_rustls_config_succeeds_on_real_pair() {
    let dir = ScratchDir::new("load_ok");
    let Some((cert, key)) = self_signed_pair(dir.path()) else {
        return skip("test_load_rustls_config_succeeds_on_real_pair");
    };
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

/// Bind the real router over TLS on an ephemeral port. Returns the address,
/// the axum-server handle, and the serve task.
async fn serve_tls(
    cert: &str,
    key: &str,
    state: orion::server::state::AppState,
) -> (
    SocketAddr,
    axum_server::Handle<SocketAddr>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let rustls_config = orion::server::tls::load_rustls_config(cert, key)
        .await
        .expect("load rustls config");
    let router = orion::server::build_router(state);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    // tokio refuses to register a blocking fd; `bind_rustls` does this itself,
    // but an ephemeral port has to come from a listener we bound ourselves.
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("addr");
    let handle = axum_server::Handle::new();
    let server_handle = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, rustls_config)
            .expect("from_tcp_rustls")
            .handle(server_handle)
            .serve(router.into_make_service_with_connect_info::<SocketAddr>())
            .await
    });
    (addr, handle, task)
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
    let Some((cert, key)) = self_signed_pair(dir.path()) else {
        return skip("test_https_round_trip");
    };

    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let (addr, handle, task) = serve_tls(&cert, &key, state).await;

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
    let Some((cert, key)) = self_signed_pair(dir.path()) else {
        return skip("test_tls_drain_withdraws_readiness_before_stopping_accept");
    };

    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let ready = state.ready.clone();
    assert!(ready.load(Ordering::Acquire));

    let (addr, handle, task) = serve_tls(&cert, &key, state).await;
    let base = format!("https://localhost:{}", addr.port());
    let client = https_client(&cert);

    // 1. Healthy before the signal.
    let resp = client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("readyz before signal");
    assert_eq!(resp.status(), 200);

    // 2. Signal: the same drain_gate main.rs uses on the TLS path.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let gate = orion::server::drain::drain_gate(
        async move {
            let _ = shutdown_rx.await;
        },
        ready.clone(),
        Duration::from_secs(2),
    );
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        gate.await;
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(5)));
    });

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
