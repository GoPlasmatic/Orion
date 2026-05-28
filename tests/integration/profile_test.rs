//! Integration tests for per-request workflow profile mode
//! (`X-Orion-Profile: 1` / `?profile=1`).

use crate::common;

use axum::http::StatusCode;
use orion::config::AppConfig;
use serde_json::json;
use tower::ServiceExt;

fn enabled_config() -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.tracing.debug_profile_enabled = true;
    cfg
}

/// Build a request matching `common::json_request` but with the
/// `X-Orion-Profile: 1` header attached.
fn json_request_with_profile(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("x-orion-profile", "1");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => axum::body::Body::from(serde_json::to_string(&v).unwrap()),
        None => axum::body::Body::empty(),
    };
    builder.body(body).unwrap()
}

#[tokio::test]
async fn profile_header_disabled_by_default() {
    // `debug_profile_enabled` defaults to false — header should be ignored.
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("pcache")).await;
    common::create_and_activate_channel(
        &app,
        "p-default",
        common::workflow_with_tasks(
            "ProfileDefault",
            json!([{
                "id": "t1", "name": "Write",
                "function": {"name": "cache_write", "input": {
                    "connector": "pcache", "key": "k", "value": "v"
                }}
            }]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request_with_profile(
            "POST",
            "/api/v1/data/p-default",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("_orion").is_none(),
        "profile field should be absent when debug_profile_enabled=false; got {body:?}"
    );
}

#[tokio::test]
async fn profile_header_enabled() {
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_connector(&app, common::cache_connector_memory("p2cache")).await;
    common::create_and_activate_channel(
        &app,
        "p-on",
        common::workflow_with_tasks(
            "ProfileOn",
            json!([
                {
                    "id": "t1", "name": "Write",
                    "function": {"name": "cache_write", "input": {
                        "connector": "p2cache", "key": "g", "value": "hello"
                    }}
                },
                {
                    "id": "t2", "name": "Read",
                    "function": {"name": "cache_read", "input": {
                        "connector": "p2cache", "key": "g", "output": "data.cached"
                    }}
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request_with_profile(
            "POST",
            "/api/v1/data/p-on",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;

    // B3: profile lives under `_orion.profile` (top-level `_orion`
    // namespace reserved for debug surfaces).
    let orion = body.get("_orion").unwrap_or_else(|| {
        panic!("expected `_orion` field on response; got {body:?}");
    });
    let profile = orion.get("profile").unwrap_or_else(|| {
        panic!("expected `_orion.profile` field on response; got {body:?}");
    });
    // Locked shape (v1): version + iterable phases.
    assert_eq!(profile["version"], 1);
    assert!(profile["totals_ms"].as_f64().unwrap() > 0.0);
    assert!(profile["phases"].is_array());
    assert!(profile["handlers"].is_array());
    let handlers = profile["handlers"].as_array().unwrap();
    assert!(
        handlers.len() >= 2,
        "expected >= 2 handler samples (cache_write + cache_read); got {handlers:?}"
    );
    let funcs: std::collections::HashSet<&str> = handlers
        .iter()
        .filter_map(|h| h["function"].as_str())
        .collect();
    assert!(funcs.contains("cache_write"));
    assert!(funcs.contains("cache_read"));

    assert!(
        profile["by_function"]["cache_write"]["count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        profile["by_connector"]["p2cache"]["count"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert!(profile["request_total_ms"].as_f64().unwrap() > 0.0);
    assert!(profile["handlers_total_ms"].as_f64().unwrap() >= 0.0);
}

#[tokio::test]
async fn profile_query_param() {
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_connector(&app, common::cache_connector_memory("p3cache")).await;
    common::create_and_activate_channel(
        &app,
        "p-q",
        common::workflow_with_tasks(
            "ProfileQuery",
            json!([{
                "id": "t1", "name": "Write",
                "function": {"name": "cache_write", "input": {
                    "connector": "p3cache", "key": "k", "value": "v"
                }}
            }]),
        ),
    )
    .await;

    // No header, just ?profile=1.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/p-q?profile=1",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body.get("_orion").and_then(|o| o.get("profile")).is_some(),
        "expected _orion.profile via ?profile=1; got {body:?}"
    );
}

#[tokio::test]
async fn profile_overhead_residual_nonnegative() {
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_connector(&app, common::cache_connector_memory("p4cache")).await;
    common::create_and_activate_channel(
        &app,
        "p-res",
        common::workflow_with_tasks(
            "ProfileResidual",
            json!([{
                "id": "t1", "name": "Write",
                "function": {"name": "cache_write", "input": {
                    "connector": "p4cache", "key": "k", "value": "v"
                }}
            }]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request_with_profile(
            "POST",
            "/api/v1/data/p-res",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    let body = common::body_json(resp).await;
    let profile = &body["_orion"]["profile"];

    let overhead = profile["workflow_overhead_ms"].as_f64().unwrap_or(0.0);
    assert!(
        overhead >= 0.0,
        "workflow_overhead_ms must be non-negative, got {overhead}"
    );

    let workflow_total = profile["workflow_total_ms"].as_f64().unwrap();
    let handlers_total = profile["handlers_total_ms"].as_f64().unwrap();
    let lock_wait = profile["engine_lock_wait_ms"].as_f64().unwrap_or(0.0);
    // Allow small floating-point slack.
    let sum = handlers_total + overhead + lock_wait;
    assert!(
        (sum - workflow_total).abs() < 2.0,
        "expected handlers_total + workflow_overhead + engine_lock_wait \u{2248} workflow_total, got {sum} vs {workflow_total}"
    );
}

#[tokio::test]
async fn profile_async_embedded_in_trace() {
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_connector(&app, common::cache_connector_memory("p5cache")).await;
    common::create_and_activate_channel(
        &app,
        "p-async",
        common::workflow_with_tasks(
            "ProfileAsync",
            json!([{
                "id": "t1", "name": "Write",
                "function": {"name": "cache_write", "input": {
                    "connector": "p5cache", "key": "k", "value": "v"
                }}
            }]),
        ),
    )
    .await;

    // Async submission with profile header.
    let resp = app
        .clone()
        .oneshot(json_request_with_profile(
            "POST",
            "/api/v1/data/p-async/async",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = common::body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();

    let final_trace = common::poll_trace_until_done(&app, &trace_id, 40).await;
    assert_eq!(final_trace["status"], "completed");

    // result_json is exposed under the `message` key by the trace polling
    // endpoint; the embedded profile lives at `message._orion.profile`
    // (B3 shape lock — same envelope as the sync response).
    let profile = &final_trace["message"]["_orion"]["profile"];
    assert!(
        profile.is_object(),
        "expected _orion.profile inside trace.message; got {final_trace}"
    );
    assert_eq!(profile["version"], 1);
    assert!(profile["handlers"].is_array());
}

#[tokio::test]
async fn profile_falsy_header_ignored() {
    // Truthy detector: "0", "false", "no" should NOT enable profiling.
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_connector(&app, common::cache_connector_memory("p6cache")).await;
    common::create_and_activate_channel(
        &app,
        "p-falsy",
        common::workflow_with_tasks(
            "ProfileFalsy",
            json!([{
                "id": "t1", "name": "Write",
                "function": {"name": "cache_write", "input": {
                    "connector": "p6cache", "key": "k", "value": "v"
                }}
            }]),
        ),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/p-falsy")
        .header("content-type", "application/json")
        .header("x-orion-profile", "0")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let body = common::body_json(resp).await;
    assert!(
        body.get("_orion").is_none(),
        "falsy header value should not enable profile; got {body:?}"
    );
}

#[tokio::test]
async fn profile_no_connector_handlers_has_no_negative_zero() {
    // With a connector-free workflow the handler durations sum to ~0; rounding
    // used to leak IEEE -0.0 into the JSON (e.g. "-0.000 ms"). All profile
    // numbers are non-negative, so "-0.0" must never appear.
    let app = common::test_app_with_config(enabled_config()).await;

    common::create_and_activate_channel(
        &app,
        "p-nozero",
        common::workflow_with_tasks(
            "ProfileNoZero",
            json!([{
                "id": "t1", "name": "Log",
                "function": {"name": "log", "input": {"message": "x"}}
            }]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request_with_profile(
            "POST",
            "/api/v1/data/p-nozero",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let profile = &body["_orion"]["profile"];
    let serialized = serde_json::to_string(profile).unwrap();
    assert!(
        !serialized.contains("-0.0"),
        "profile must not serialize negative zero; got {serialized}"
    );
}
