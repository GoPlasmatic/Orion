//! The model fixture and the bucket that stands in for its object storage.
//!
//! `tests/fixtures/models/c4-tiny/` holds a 6171-byte ONNX graph and its
//! manifest; `build.py` there regenerates the graph. A test that registers
//! a model needs three things: a bucket answering signed `HEAD` and `GET`
//! for the object, a `storage` connector pointing at it through the admin
//! API, and the digest the registration claims — all of which are here so
//! the model tests and the route-contract tests that include models share
//! one spelling.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{body_json, json_request};
use orion::config::AppConfig;

/// The fixture graph, as the bucket serves it and as the digest is computed.
pub const FIXTURE_ONNX: &[u8] = include_bytes!("../../fixtures/models/c4-tiny/c4-tiny.onnx");
/// The fixture manifest, as a registration carries it.
pub const FIXTURE_MANIFEST: &str = include_str!("../../fixtures/models/c4-tiny/model.json");
/// The model id the manifest declares.
pub const FIXTURE_ID: &str = "ada.c4-tiny";
/// The object key the bucket serves the graph under.
pub const FIXTURE_KEY: &str = "models/c4-tiny.onnx";

/// `sha256:…` of [`FIXTURE_ONNX`] — what a registration claims.
pub fn fixture_digest() -> String {
    orion::crypto::sha256_digest(FIXTURE_ONNX)
}

/// The manifest as a JSON value.
pub fn manifest() -> Value {
    serde_json::from_str(FIXTURE_MANIFEST).expect("fixture manifest parses")
}

/// A bucket standing in for the connector's: serves `body` under every key,
/// counts the GETs, and asserts every request arrived SigV4-signed.
pub struct Bucket {
    pub addr: std::net::SocketAddr,
    pub gets: Arc<AtomicUsize>,
    pub heads: Arc<AtomicUsize>,
}

pub async fn spawn_bucket(body: Vec<u8>) -> Bucket {
    use axum::http::HeaderMap;
    let gets = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(AtomicUsize::new(0));
    let body = Arc::new(body);
    let assert_signed = |headers: &HeaderMap| {
        assert!(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 Credential=")),
            "the request must arrive signed"
        );
    };
    let head_body = body.clone();
    let head_count = heads.clone();
    let head = move |headers: HeaderMap| async move {
        assert_signed(&headers);
        head_count.fetch_add(1, Ordering::SeqCst);
        let mut out = HeaderMap::new();
        out.insert(
            "content-length",
            head_body.len().to_string().parse().expect("len"),
        );
        out.insert("etag", "\"fixture-etag\"".parse().expect("etag"));
        (StatusCode::OK, out)
    };
    let get_count = gets.clone();
    let get = move |headers: HeaderMap| async move {
        assert_signed(&headers);
        get_count.fetch_add(1, Ordering::SeqCst);
        (StatusCode::OK, body.to_vec())
    };
    let app = axum::Router::new().route("/{*path}", axum::routing::get(get).head(head));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Bucket { addr, gets, heads }
}

/// A `storage` connector body pointing at `addr`, path-style, private
/// addresses allowed — the shape a self-hosted store needs and a test on
/// localhost must have.
pub fn storage_connector(name: &str, addr: std::net::SocketAddr) -> Value {
    json!({
        "name": name,
        "connector_type": "storage",
        "config": {
            "endpoint": format!("http://{addr}"),
            "region": "us-east-1",
            "bucket": "models",
            "access_key": "AKIAFIXTURE",
            "secret_key": "fixture-secret",
            "force_path_style": true,
            "allow_private_urls": true
        }
    })
}

/// Create `storage_connector(name, addr)` through the admin API.
pub async fn create_storage_connector(app: &axum::Router, name: &str, addr: std::net::SocketAddr) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(storage_connector(name, addr)),
        ))
        .await
        .expect("request");
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// A registration of the fixture through `connector`, claiming `digest`.
pub fn registration(connector: &str, digest: &str) -> Value {
    json!({
        "manifest": manifest(),
        "artifact": {
            "connector": connector,
            "key": FIXTURE_KEY,
            "digest": digest,
        },
        "tags": ["fixture"]
    })
}

/// A config with models enabled and a fresh cache directory per call, so
/// two tests never share (or sweep) each other's cache.
pub fn models_config(enabled: bool) -> AppConfig {
    let mut config = AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    config.models.enabled = enabled;
    config.models.cache_dir = std::env::temp_dir()
        .join(format!("orion-model-test-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned();
    config
}

/// A bucket serving the fixture, an app with models enabled, and a storage
/// connector named `bucket` pointing at it: everything a registration
/// needs.
pub struct ModelHarness {
    pub state: orion::server::state::AppState,
    pub app: axum::Router,
    pub bucket: Bucket,
}

pub async fn harness() -> ModelHarness {
    harness_with(models_config(true)).await
}

/// [`harness`] over a config of the caller's — `models_config(true)` with
/// a ceiling changed, typically.
pub async fn harness_with(config: AppConfig) -> ModelHarness {
    let bucket = spawn_bucket(FIXTURE_ONNX.to_vec()).await;
    let state = super::test_state_with_config(config).await;
    let app = orion::server::build_router(state.clone());
    create_storage_connector(&app, "bucket", bucket.addr).await;
    ModelHarness { state, app, bucket }
}

/// Register the fixture and hand back the `202` body's `data`.
pub async fn register_fixture(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/models",
            Some(registration("bucket", &fixture_digest())),
        ))
        .await
        .expect("request");
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    body["data"].clone()
}

/// Run this node's admission for the latest version of `id` inline, the way
/// the worker would, and hand back the outcome.
pub async fn admit(
    state: &orion::server::state::AppState,
    id: &str,
) -> orion::model::AdmissionOutcome {
    let row = state.repos.models.get_by_id(id).await.expect("model row");
    let job = orion::runtime::model_admission::job_for(&row).expect("job");
    orion::runtime::model_admission::admit_now(state, job)
        .await
        .expect("admission recorded")
}
