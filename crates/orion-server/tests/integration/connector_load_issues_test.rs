//! F16: a deployed connection that fails to load must be visible.
//!
//! A connection whose config cannot be resolved is skipped and simply absent
//! from the connector registry, so every workflow using it returns a 500 at
//! request time — possibly hours after the deployment that broke it. Before
//! this, a log line was the only evidence. These tests pin the reporting
//! surface, `/health`.

use crate::common::{self, body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// The variables this module's tests resolve through `env://`, set before
/// `main()` while the process is single-threaded: the only point where
/// `set_var` cannot race another test's `getenv`. Each is uniquely named and
/// in the reserved `ORION_SECRET_*` namespace, so a test that loads config
/// does not refuse it as a misspelled override.
#[ctor::ctor(unsafe)]
fn install_connector_load_env() {
    // SAFETY: runs pre-main on the sole thread of the process.
    unsafe {
        std::env::set_var(
            "ORION_SECRET_T338_BAD_SCHEME_URL",
            "ftp://user:hunter2@files.example.com/",
        );
        std::env::set_var("ORION_SECRET_T338_PEER_URL", "http://127.0.0.1:9");
        std::env::set_var("ORION_SECRET_T338_PEER_PRIVATE", "true");
        std::env::set_var("ORION_SECRET_T338_NOT_A_BOOL", "sk_live_hunter2");
    }
}

/// Deploy a connection whose secret reference cannot resolve. The
/// deployment converges — the reference is only resolved when the registry
/// loads it.
async fn create_broken_connector(app: &axum::Router, id: &str) {
    common::create_connector(
        app,
        json!({
            "name": id,
            "connector_type": "http",
            "config": {
                "url": "https://api.example.com",
                "method": "GET",
                "auth": {"type": "bearer", "token": "env://ORION_F16_DEFINITELY_UNSET"}
            }
        }),
    )
    .await;
}

#[tokio::test]
async fn health_reports_a_connector_that_failed_to_load() {
    let app = test_app().await;
    create_broken_connector(&app, "f16-broken").await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    // Degraded, not down: the rest of the instance still serves traffic, and
    // a 503 would pull the node out of its load balancer.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    assert_eq!(body["status"], "degraded");
    assert_eq!(body["components"]["connectors"], "degraded");
    assert_eq!(body["components"]["database"], "ok");
    assert_eq!(body["components"]["engine"], "ok");

    let failures = body["connectors"]["failed_to_load"].as_array().unwrap();
    assert_eq!(
        failures.len(),
        1,
        "expected exactly one failure: {failures:?}"
    );
    assert_eq!(failures[0]["connector"], "f16-broken");
    assert_eq!(failures[0]["stage"], "secret_resolution");
    assert!(
        failures[0]["reason"]
            .as_str()
            .unwrap()
            .contains("ORION_F16_DEFINITELY_UNSET"),
        "the reason must name the unresolvable reference: {}",
        failures[0]["reason"]
    );
}

#[tokio::test]
async fn health_is_ok_when_every_connector_loads() {
    let app = test_app().await;

    common::create_connector(
        &app,
        json!({
            "name": "f16-fine",
            "connector_type": "http",
            "config": {"url": "https://api.example.com", "method": "GET"}
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["components"]["connectors"], "ok");
    assert!(
        body["connectors"]["failed_to_load"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// The failure must clear once a fixed config is deployed, not stick until
/// restart.
#[tokio::test]
async fn fixing_the_config_clears_the_issue() {
    let app = test_app().await;
    create_broken_connector(&app, "f16-fixable").await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["status"], "degraded");

    // The fix is the connection's next revision, deployed.
    common::create_connector(
        &app,
        json!({
            "name": "f16-fixable",
            "connector_type": "http",
            "config": {"url": "https://api.example.com", "method": "GET"}
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(
        body["connectors"]["failed_to_load"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the issue list is rebuilt from each deployment, not accumulated"
    );
}

/// A connection carries no type: the driver it names decides it. A config
/// authored the documented way — no `"type"` inside it — must load as the
/// driver's type. (In v1, where the type was a column beside the config,
/// this shape once failed with "missing field `type`" and silently never
/// loaded; it was the bug F16's reporting surface made visible on its first
/// run.)
#[tokio::test]
async fn a_config_without_a_redundant_inner_type_loads() {
    let app = test_app().await;

    for (id, connector_type, config) in [
        (
            "f16-http-plain",
            "http",
            json!({"url": "https://api.example.com", "method": "GET"}),
        ),
        ("f16-cache-plain", "cache", json!({"backend": "memory"})),
        (
            "f16-db-plain",
            "db",
            json!({"connection_string": "sqlite::memory:"}),
        ),
    ] {
        common::create_connector(
            &app,
            json!({"name": id, "connector_type": connector_type, "config": config}),
        )
        .await;
    }

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["connectors"]["failed_to_load"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new(),
        "no connector should need a redundant inner `type` to load"
    );
}

/// An inner `type` that contradicts the connection's driver must not win —
/// the driver is the source of truth, so a memory-cache connection whose
/// public config says `"type": "http"` still loads as a cache.
#[tokio::test]
async fn the_driver_wins_over_an_inner_type() {
    let app = test_app().await;
    deploy_raw_connection(
        &app,
        "f16-conflict",
        "org.orion.memory-cache",
        json!({"type": "http", "backend": "memory"}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert!(
        body["connectors"]["failed_to_load"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the cache config must load as a cache, not fail as a malformed http: {}",
        body["connectors"]["failed_to_load"]
    );
}

/// Create a connection over the built-in `driver` with `public_config`
/// exactly as given — no secret split, no key stripped — and deploy it.
async fn deploy_raw_connection(
    app: &axum::Router,
    id: &str,
    driver: &str,
    public_config: serde_json::Value,
) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/runtime-capabilities",
            None,
        ))
        .await
        .unwrap();
    let caps = body_json(resp).await;
    let provider = caps["provider_backends"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["driver_id"] == driver))
        .and_then(|b| b["package_digest"].as_str())
        .unwrap_or_else(|| panic!("no built-in digest for {driver}: {caps}"))
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v2/admin/connections/{id}/revisions"),
            Some(json!({
                "revision": 1,
                "provider_digest": provider,
                "public_config": public_config,
                "secret_locators": {},
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let digest = created["connection"]["configuration_digest"]
        .as_str()
        .expect("a digest")
        .to_string();
    common::deploy::deploy(
        app,
        vec![common::deploy::Upsert {
            kind: "connection",
            id: id.to_string(),
            digest,
            revision: Some(1),
            requires: Vec::new(),
        }],
    )
    .await;
}

/// A connector naming a type this build has no variant for — `storage`, a
/// 0.x type — does not load: it reports a load issue naming the type and the
/// ones this build knows, rather than failing the load or loading as a
/// connector nothing can use.
#[tokio::test]
async fn a_stored_storage_connector_reports_an_unknown_type() {
    let registry = orion::connector::ConnectorRegistry::new(Default::default());
    registry
        .load(vec![orion::connector::StoredConnector {
            id: "legacy-uploads@1".to_string(),
            name: "legacy-uploads".to_string(),
            connector_type: "storage".to_string(),
            config_json: r#"{"provider":"s3","bucket":"my-uploads","region":"us-east-1"}"#
                .to_string(),
        }])
        .await;

    let issues = registry.load_issues().await;
    assert_eq!(issues.len(), 1, "expected one load issue: {issues:?}");
    assert_eq!(issues[0].connector, "legacy-uploads");
    assert!(
        issues[0]
            .reason
            .contains("unknown connector type 'storage'")
            && issues[0].reason.contains("http, db, cache"),
        "the reason must name the type and the known ones: {}",
        issues[0].reason
    );
}

/// Deploy an http connection with `config`. A reference in it is judged
/// when the registry loads it, so the deployment itself converges.
async fn create_http(app: &axum::Router, name: &str, config: serde_json::Value) {
    common::create_connector(
        app,
        json!({"name": name, "connector_type": "http", "config": config}),
    )
    .await;
}

/// The `failed_to_load` entry `/health` reports for `name`, if any.
async fn load_failure(app: &axum::Router, name: &str) -> Option<serde_json::Value> {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    body["connectors"]["failed_to_load"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["connector"] == name)
        .cloned()
}

/// #338: nothing judged an endpoint after its reference resolved, so an
/// `env://` URL resolving to `ftp://…` reached the client unjudged. It is
/// now an `endpoint` load issue naming the scheme — and never the value,
/// which may carry userinfo.
#[tokio::test]
async fn a_reference_url_resolving_to_a_bad_scheme_is_an_endpoint_load_issue() {
    let app = test_app().await;
    create_http(
        &app,
        "t338-ftp",
        json!({"url": "env://ORION_SECRET_T338_BAD_SCHEME_URL"}),
    )
    .await;
    let failure = load_failure(&app, "t338-ftp").await.expect("a load issue");
    assert_eq!(failure["stage"], "endpoint", "{failure}");
    let reason = failure["reason"].as_str().unwrap();
    assert!(reason.contains("'ftp'"), "{reason}");
    assert!(!reason.contains("hunter2"), "{reason}");
}

/// A boolean opt-out may be a reference: `true` or `false` is the boolean,
/// and the connector loads with it.
#[tokio::test]
async fn a_boolean_reference_resolving_to_true_loads() {
    let app = test_app().await;
    create_http(
        &app,
        "t338-peer",
        json!({
            "url": "env://ORION_SECRET_T338_PEER_URL",
            "allow_private_urls": "env://ORION_SECRET_T338_PEER_PRIVATE",
        }),
    )
    .await;
    assert!(load_failure(&app, "t338-peer").await.is_none());
}

/// Anything else there is refused at load, naming the field and withholding
/// what the reference resolved to — it may be a credential a reference was
/// mis-pointed at.
#[tokio::test]
async fn a_boolean_reference_resolving_to_garbage_is_a_deserialize_load_issue() {
    let app = test_app().await;
    create_http(
        &app,
        "t338-garbage",
        json!({
            "url": "https://api.example.com",
            "allow_private_urls": "env://ORION_SECRET_T338_NOT_A_BOOL",
        }),
    )
    .await;
    let failure = load_failure(&app, "t338-garbage")
        .await
        .expect("a load issue");
    assert_eq!(failure["stage"], "deserialize", "{failure}");
    let reason = failure["reason"].as_str().unwrap();
    assert!(reason.contains("allow_private_urls"), "{reason}");
    assert!(!reason.contains("hunter2"), "{reason}");
}
