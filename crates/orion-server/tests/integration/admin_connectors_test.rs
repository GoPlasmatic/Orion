//! Connectors at run time: connections deployed over the built-in drivers,
//! their endpoint gates, the circuit-breaker admin endpoints, and probing.
//!
//! A connection's public config is not shape-checked when it is created; a
//! connector the node cannot load is reported in `/health` under
//! `connectors.failed_to_load`, naming the stage that refused it.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The `/health` load issue for connector `name`, if it failed to load.
async fn load_issue(app: &axum::Router, name: &str) -> Option<Value> {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let health = body_json(resp).await;
    health["connectors"]["failed_to_load"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|issue| issue["connector"] == name)
        .cloned()
}

/// Deploy a v1-shaped connector body as a connection.
async fn deploy_connector(app: &axum::Router, id: &str, connector_type: &str, config: Value) {
    common::create_connector(
        app,
        json!({"name": id, "connector_type": connector_type, "config": config}),
    )
    .await;
}

// ============================================================
// Circuit Breaker Admin Endpoints
// ============================================================

#[tokio::test]
async fn test_circuit_breaker_list_endpoint() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["enabled"], false);
    assert!(body["data"].get("breakers").is_some());
}

#[tokio::test]
async fn test_circuit_breaker_reset_not_found() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/connectors/circuit-breakers/nonexistent-key",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
}

// ============================================================
// Data sync processing with metadata
// ============================================================

#[tokio::test]
async fn test_sync_processing_with_metadata() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "meta-ch",
        json!({
            "name": "Metadata Workflow",
            "condition": true,
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "meta test"}}}]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/meta-ch",
            Some(json!({
                "data": {"order_id": "ORD-001"},
                "metadata": {"source": "api", "trace_id": "abc123"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body.get("id").is_some());
    assert!(body.get("data").is_some());
    assert!(body.get("errors").is_some());
}

// ---------------------------------------------------------------------------
// S6: every connector variant's endpoint is scheme-checked when the node loads
// it, and address-checked when it is first dialled. Before 1.0 only `http`
// was gated, so a db connector holding `postgres://…@169.254.169.254/…` was
// accepted and connected to.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_http_connector_with_a_non_http_url_scheme_does_not_load() {
    let app = common::test_app().await;

    deploy_connector(
        &app,
        "ftp-connector",
        "http",
        json!({"url": "ftp://example.com/files", "method": "GET"}),
    )
    .await;

    let issue = load_issue(&app, "ftp-connector")
        .await
        .expect("an ftp:// http connector must not load");
    assert_eq!(issue["stage"], "endpoint", "{issue}");
    assert!(
        issue["reason"].as_str().unwrap_or("").contains("ftp"),
        "the issue must name the scheme: {issue}"
    );
}

#[tokio::test]
async fn s6_db_connector_rejects_schemes_that_are_not_databases() {
    let app = common::test_app().await;

    for (id, conn) in [
        ("s6-db-http", "http://169.254.169.254/latest/meta-data"),
        ("s6-db-file", "file:///etc/passwd"),
        ("s6-db-redis", "redis://cache.example.com:6379"),
        ("s6-db-gopher", "gopher://example.com:70/"),
        ("s6-db-bare", "/var/lib/orion/orion.db"),
        // MongoDB is not a Community backend.
        ("s6-db-mongo", "mongodb://m.example.com:27017/orion"),
        ("s6-db-srv", "mongodb+srv://cluster.example.com/orion"),
    ] {
        // The driver is named, since the scheme cannot say which it is.
        deploy_connector(
            &app,
            id,
            "db",
            json!({"driver": "postgresql", "connection_string": conn}),
        )
        .await;
        let issue = load_issue(&app, id)
            .await
            .unwrap_or_else(|| panic!("{conn} must not load"));
        assert!(
            issue["reason"].as_str().unwrap_or("").contains("Allowed:"),
            "{conn}: the issue should name the allowed schemes, got {issue}"
        );
    }
}

#[tokio::test]
async fn s6_db_connector_accepts_every_real_backend_scheme() {
    let app = common::test_app().await;

    for (id, conn) in [
        ("s6-ok-pg", "postgres://db.example.com/orion"),
        ("s6-ok-pgsql", "postgresql://db.example.com/orion"),
        ("s6-ok-mysql", "mysql://db.example.com/orion"),
        ("s6-ok-sqlite", "sqlite::memory:"),
    ] {
        deploy_connector(&app, id, "db", json!({"connection_string": conn})).await;
        let issue = load_issue(&app, id).await;
        assert!(issue.is_none(), "{conn} must load: {issue:?}");
    }
}

#[tokio::test]
async fn a_cache_connector_is_memory_only() {
    let app = common::test_app().await;

    // `redis` is refused like any unsupported backend, naming the one there is.
    deploy_connector(
        &app,
        "cache-redis",
        "cache",
        json!({"backend": "redis", "url": "rediss://cache.example.com:6379"}),
    )
    .await;
    let issue = load_issue(&app, "cache-redis")
        .await
        .expect("a redis cache connector must not load");
    assert!(
        issue["reason"]
            .as_str()
            .unwrap_or("")
            .contains("Invalid cache backend 'redis'. Must be one of: memory"),
        "{issue}"
    );

    deploy_connector(&app, "cache-memory", "cache", json!({"backend": "memory"})).await;
    let issue = load_issue(&app, "cache-memory").await;
    assert!(issue.is_none(), "a memory cache connector loads: {issue:?}");
}

/// The scheme gate is not the address gate: a perfectly well-formed
/// `postgres://` URL aimed at the cloud metadata endpoint is stored happily
/// and refused when the pool is opened.
#[tokio::test]
async fn s6_private_db_target_is_refused_when_the_pool_is_opened() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        json!({
            "id": "s6-metadata",
            "name": "s6-metadata",
            "connector_type": "db",
            "config": {
                "type": "db",
                "connection_string": "postgres://169.254.169.254:5432/orion",
                "connect_timeout_ms": 1000
            }
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "s6-ch",
        common::workflow_with_tasks(
            "s6",
            json!([{
                "id": "r", "name": "r",
                "on_failure": "continue",
                "function": { "name": "db_read", "input": {
                    "connector": "s6-metadata",
                    "query": "SELECT 1",
                    "output": "data.rows"
                }}
            }]),
        ),
    )
    .await;

    let (_, body) = common::dsl::post(&app, "s6-ch", json!({ "data": {} })).await;

    // The task must have failed — the connection was refused, not made.
    assert!(
        !body["errors"].as_array().is_none_or(|e| e.is_empty()),
        "db_read against a link-local target must fail: {body}"
    );

    // G3 keeps the detail off the anonymous data plane ("full detail is
    // available in the trace"), so the message that names the target and the
    // opt-out is asserted where it actually lives.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?channel=s6-ch&limit=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let trace_id = list["data"][0]["id"].as_str().expect("trace id");

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rendered = body_json(resp).await.to_string();
    assert!(
        rendered.contains("169.254.169.254") && rendered.contains("allow_private_urls"),
        "the trace must name the refused target and the opt-out: {rendered}"
    );
}

/// ...and the opt-out has to actually work, or every private-network
/// deployment is broken.
#[tokio::test]
async fn s6_allow_private_urls_lets_a_loopback_db_through() {
    let app = common::test_app().await;

    // sqlite has no host, so use it to prove the *scheme* path stays open;
    // the address path is proved by the container suites, which all run
    // against 127.0.0.1 with allow_private_urls set.
    common::create_connector(
        &app,
        common::db_connector_sqlite("s6-local", "sqlite:file:s6_local?mode=memory&cache=shared"),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "s6-local-ch",
        common::workflow_with_tasks(
            "s6-local",
            json!([{
                "id": "r", "name": "r",
                "function": { "name": "db_read", "input": {
                    "connector": "s6-local",
                    "query": "SELECT 1 AS one",
                    "output": "data.rows"
                }}
            }]),
        ),
    )
    .await;

    let (status, body) = common::dsl::post(&app, "s6-local-ch", json!({ "data": {} })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ============================================================
// Connectivity probe: POST /connections/{id}/probe
// ============================================================

#[tokio::test]
async fn probing_an_unknown_connection_is_404() {
    let app = common::test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/connections/no-such-connector/probe",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONNECTION_NOT_FOUND", "{body}");
}
