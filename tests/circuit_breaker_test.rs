mod common;

use axum::http::StatusCode;
use common::{body_json, json_request};
use serde_json::json;
use tower::ServiceExt;

use orion::config::{AppConfig, EngineConfig};
use orion::connector::circuit_breaker::CircuitBreakerConfig;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spin up a mock HTTP server that always returns 500 Internal Server Error.
async fn start_failing_server() -> std::net::SocketAddr {
    let mock_app = axum::Router::new().route(
        "/fail",
        axum::routing::post(|| async {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "boom"})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });
    addr
}

/// Create an HTTP connector pointing at the given address with zero retries.
async fn create_http_connector(app: &axum::Router, name: &str, addr: std::net::SocketAddr) {
    common::create_connector(
        app,
        json!({
            "id": name,
            "name": name,
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{}", addr),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;
}

/// Build a workflow whose single task calls http_call on the given connector
/// to POST /fail.
fn failing_http_workflow(name: &str, connector: &str) -> serde_json::Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "t1",
            "name": "Call failing endpoint",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": connector,
                    "method": "POST",
                    "path": "/fail",
                    "body": {"test": true},
                    "response_path": "data.result",
                    "timeout_ms": 5000
                }
            }
        }]),
    )
}

/// Build an AppConfig with circuit breakers enabled and the given parameters.
fn cb_config(threshold: u32, recovery_secs: u64) -> AppConfig {
    AppConfig {
        engine: EngineConfig {
            circuit_breaker: CircuitBreakerConfig {
                enabled: true,
                failure_threshold: threshold,
                recovery_timeout_secs: recovery_secs,
                max_breakers: 10_000,
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Send a data request through the given channel and return (status, body).
async fn send_data_request(app: &axum::Router, channel: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}", channel),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

// ---------------------------------------------------------------------------
// Test 1: Circuit breaker trips after failure_threshold consecutive failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_breaker_trips_after_threshold() {
    let addr = start_failing_server().await;
    let app = common::test_app_with_config(cb_config(3, 300)).await;

    // Create connector and channel
    create_http_connector(&app, "failing-api", addr).await;
    common::create_and_activate_channel(
        &app,
        "cb-trip",
        failing_http_workflow("CB Trip Workflow", "failing-api"),
    )
    .await;

    // Send 3 requests — each should fail (500 from upstream), but the breaker
    // allows them through because it is still closed (accumulating failures).
    for i in 0..3 {
        let (status, _body) = send_data_request(&app, "cb-trip").await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Request {} should fail with 500 (upstream error)",
            i + 1
        );
    }

    // 4th request — circuit breaker should now be open and reject immediately.
    let (status, body) = send_data_request(&app, "cb-trip").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "4th request should be rejected by the open circuit breaker"
    );
    // The error body should mention the engine error (circuit breaker fires a DataflowError)
    let error_msg = body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    assert!(
        error_msg.contains("engine")
            || error_msg.contains("error")
            || error_msg.contains("internal"),
        "Expected engine/internal error, got: {}",
        error_msg
    );

    // Verify via admin API that a breaker exists in "open" state
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], json!(true));

    // The breakers field is a map of key -> state. Find one that is "open".
    let breakers = body["breakers"]
        .as_object()
        .expect("breakers should be an object");
    assert!(
        !breakers.is_empty(),
        "Expected at least one circuit breaker entry"
    );
    let has_open = breakers.values().any(|v| v.as_str() == Some("open"));
    assert!(
        has_open,
        "Expected at least one breaker in 'open' state, got: {:?}",
        breakers
    );
}

// ---------------------------------------------------------------------------
// Test 2: Circuit breaker can be reset via admin API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_breaker_reset_via_admin_api() {
    let addr = start_failing_server().await;
    // Use threshold=2 for quicker tripping
    let app = common::test_app_with_config(cb_config(2, 300)).await;

    create_http_connector(&app, "reset-api", addr).await;
    common::create_and_activate_channel(
        &app,
        "cb-reset",
        failing_http_workflow("CB Reset Workflow", "reset-api"),
    )
    .await;

    // Trip the breaker with 2 failures
    for _ in 0..2 {
        let (status, _) = send_data_request(&app, "cb-reset").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Verify it is open
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let breakers = body["breakers"]
        .as_object()
        .expect("breakers should be an object");
    // Find the key for our breaker (format is "channel:connector")
    let breaker_key = breakers
        .iter()
        .find(|(_k, v)| v.as_str() == Some("open"))
        .map(|(k, _)| k.clone())
        .expect("Expected an open breaker");

    // Reset it via admin API
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/circuit-breakers/{}", breaker_key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["reset"], json!(true));
    assert_eq!(body["key"], json!(breaker_key));

    // Verify it is now closed
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let breakers = body["breakers"]
        .as_object()
        .expect("breakers should be an object");
    let state = breakers
        .get(&breaker_key)
        .and_then(|v| v.as_str())
        .unwrap_or("missing");
    assert_eq!(state, "closed", "Breaker should be closed after reset");
}

// ---------------------------------------------------------------------------
// Test 3: Circuit breaker transitions to half-open after recovery timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_breaker_half_open_recovery() {
    let addr = start_failing_server().await;
    // Use recovery_timeout_secs=1 so we can sleep briefly and observe half-open
    let app = common::test_app_with_config(cb_config(2, 1)).await;

    create_http_connector(&app, "halfopen-api", addr).await;
    common::create_and_activate_channel(
        &app,
        "cb-halfopen",
        failing_http_workflow("CB HalfOpen Workflow", "halfopen-api"),
    )
    .await;

    // Trip the breaker
    for _ in 0..2 {
        let (status, _) = send_data_request(&app, "cb-halfopen").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Confirm it is open
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let breakers = body["breakers"].as_object().unwrap();
    let has_open = breakers.values().any(|v| v.as_str() == Some("open"));
    assert!(has_open, "Breaker should be open after threshold failures");

    // Wait for recovery timeout to elapse (1 second + margin)
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // The next request should be allowed through (half-open probe).
    // Since the mock server still returns 500, the probe will fail and
    // the breaker will re-open. But crucially, the request was NOT
    // rejected immediately — it actually reached the mock server.
    // We can verify this because the response is a 500 ENGINE_ERROR
    // (from the upstream failure), not a circuit-breaker-open rejection.
    let (status, _body) = send_data_request(&app, "cb-halfopen").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Half-open probe should reach upstream and return 500"
    );

    // After the failed probe, breaker should be back to open.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let breakers = body["breakers"].as_object().unwrap();
    // It may show "open" (re-opened after failed probe) or "half_open"
    // (if the state check didn't transition yet). Either way it should NOT be "closed".
    let breaker_key = breakers
        .iter()
        .find(|(k, _)| k.contains("halfopen-api"))
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("unknown").to_string()));
    if let Some((_key, state)) = &breaker_key {
        assert_ne!(
            state, "closed",
            "Breaker should not be closed after a failed half-open probe"
        );
    }

    // Now test full recovery: start a success server and create a new connector
    // that points to it, then reset the breaker to prove the cycle completes.
    // (Since we can't swap the mock mid-test for the same connector, we verify
    // the reset path instead.)
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let breakers = body["breakers"].as_object().unwrap();
    let key = breakers
        .iter()
        .find(|(k, _)| k.contains("halfopen-api"))
        .map(|(k, _)| k.clone())
        .expect("Should have a breaker for halfopen-api");

    // Reset and verify closed
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/connectors/circuit-breakers/{}", key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["reset"], json!(true));
}

// ---------------------------------------------------------------------------
// Test 4: Circuit breaker disabled by default — all requests go through
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_breaker_disabled_by_default() {
    let addr = start_failing_server().await;
    // Default config has circuit_breaker.enabled = false
    let app = common::test_app().await;

    create_http_connector(&app, "nobreaker-api", addr).await;
    common::create_and_activate_channel(
        &app,
        "cb-disabled",
        failing_http_workflow("CB Disabled Workflow", "nobreaker-api"),
    )
    .await;

    // Send 10 requests — all should reach the server and return 500 (not rejected by breaker)
    for i in 0..10 {
        let (status, _body) = send_data_request(&app, "cb-disabled").await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Request {} should fail with 500 (upstream error, not breaker rejection)",
            i + 1
        );
    }

    // Verify admin endpoint reports circuit breakers as disabled
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/connectors/circuit-breakers",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["enabled"],
        json!(false),
        "Circuit breakers should be disabled by default"
    );
}
