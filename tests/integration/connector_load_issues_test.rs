//! F16: an enabled connector that fails to load must be visible.
//!
//! A connector whose config cannot be resolved is skipped and simply absent
//! from the registry, so every workflow using it returns a 500 at request
//! time — possibly hours after the deploy that broke it. Before this, a log
//! line at boot was the only evidence. These tests pin the two reporting
//! surfaces: `/health` and `GET /api/v1/admin/connectors`.

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Create a connector whose secret reference cannot resolve. Creating it
/// succeeds — the reference is only resolved when the registry loads it.
async fn create_broken_connector(app: &axum::Router, id: &str) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": id,
                "name": id,
                "connector_type": "http",
                "config": {
                    "url": "https://api.example.com",
                    "method": "GET",
                    "auth": {"type": "bearer", "token": "env://ORION_F16_DEFINITELY_UNSET"}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the reference is resolved at load, not at create"
    );
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

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f16-fine",
                "name": "f16-fine",
                "connector_type": "http",
                "config": {"url": "https://api.example.com", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

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

#[tokio::test]
async fn admin_list_marks_the_failed_connector() {
    let app = test_app().await;
    create_broken_connector(&app, "f16-listed").await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f16-healthy",
                "name": "f16-healthy",
                "connector_type": "http",
                "config": {"url": "https://api.example.com", "method": "GET"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let rows = body["data"].as_array().unwrap();

    let broken = rows
        .iter()
        .find(|r| r["name"] == "f16-listed")
        .expect("the broken connector must still be listed — it exists, it just is not live");
    assert_eq!(broken["load_status"], "failed");
    assert_eq!(broken["load_error_stage"], "secret_resolution");
    assert!(broken["load_error"].is_string());

    let healthy = rows
        .iter()
        .find(|r| r["name"] == "f16-healthy")
        .expect("healthy connector missing from the list");
    assert_eq!(healthy["load_status"], "loaded");
    assert!(healthy.get("load_error").is_none());
}

/// A disabled connector is not a failure — it was never meant to be live, so
/// it must not make `/health` degraded.
#[tokio::test]
async fn a_disabled_connector_is_not_a_load_failure() {
    let app = test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f16-disabled",
                "name": "f16-disabled",
                "connector_type": "http",
                "config": {
                    "url": "https://api.example.com",
                    "method": "GET",
                    "auth": {"type": "bearer", "token": "env://ORION_F16_DEFINITELY_UNSET"}
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f16-disabled",
            Some(json!({"enabled": false})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["status"], "ok",
        "a disabled connector is not loaded on purpose, so it is not degraded"
    );

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let row = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "f16-disabled")
        .expect("disabled connector missing from the list");
    assert_eq!(row["load_status"], "disabled");
}

/// The failure must clear once the config is fixed, not stick until restart.
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

    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/connectors/f16-fixable",
            Some(json!({"config": {
                "url": "https://api.example.com",
                "method": "GET"
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

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
        "the issue list is rebuilt on reload, not accumulated"
    );
}

/// `ConnectorConfig` is internally tagged on `type`, but the API takes the
/// type as a sibling `connector_type` field and stores it in its own column.
/// A connector authored the documented way — no redundant `"type"` inside
/// `config` — used to fail deserialization with "missing field `type`" and
/// silently never load. That is the shape every example, the OpenAPI spec and
/// any admin UI produces, so in practice most connectors were dead.
///
/// This is the bug F16's reporting surface made visible on its first run.
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
            json!({"connection_string": "sqlite::memory:", "driver": "sqlite"}),
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/connectors",
                Some(json!({
                    "id": id,
                    "name": id,
                    "connector_type": connector_type,
                    "config": config
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "create {id}");
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

/// An inner `type` that contradicts the stored column must not win — the
/// column is what create/update validated against (R4), so it is the source
/// of truth and the injection overwrites.
#[tokio::test]
async fn the_stored_type_column_wins_over_an_inner_type() {
    let app = test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "f16-conflict",
                "name": "f16-conflict",
                "connector_type": "cache",
                "config": {"type": "http", "backend": "memory"}
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

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
