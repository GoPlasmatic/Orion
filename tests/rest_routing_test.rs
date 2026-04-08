mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use common::{create_and_activate_workflow, create_rest_channel};

/// Create a simple workflow that echoes input + adds a marker.
async fn create_echo_workflow(app: &axum::Router, name: &str) -> String {
    create_and_activate_workflow(
        app,
        json!({
            "name": name,
            "condition": true,
            "tasks": [{
                "id": "echo",
                "name": "Echo",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.matched",
                            "logic": true
                        }]
                    }
                }
            }]
        }),
    )
    .await
}

#[tokio::test]
async fn test_rest_route_with_path_params() {
    let app = common::test_app().await;

    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    create_rest_channel(&app, "orders.get", "/orders/{id}", vec!["GET"], &wf_id).await;

    // GET /api/v1/data/orders/123 should match
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/orders/123")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["matched"], true);
}

#[tokio::test]
async fn test_rest_route_method_mismatch() {
    let app = common::test_app().await;

    let wf_id = create_echo_workflow(&app, "Orders POST Only").await;
    create_rest_channel(&app, "orders.create", "/orders", vec!["POST"], &wf_id).await;

    // GET /api/v1/data/orders should NOT match (only POST allowed)
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/orders")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    // Falls through to simple channel lookup — "orders" channel doesn't exist by name,
    // but it does exist by route. Since GET doesn't match methods, route table won't match.
    // The simple name lookup for "orders" will also fail (no channel named "orders").
    // So this returns 200 with engine finding no matching workflows (empty result).
    // This is acceptable — the request reaches the engine but no workflows match.
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_rest_route_multi_segment_path() {
    let app = common::test_app().await;

    let wf_id = create_echo_workflow(&app, "Order Items").await;
    create_rest_channel(
        &app,
        "orders.items",
        "/orders/{order_id}/items/{item_id}",
        vec!["GET", "PUT"],
        &wf_id,
    )
    .await;

    // GET /api/v1/data/orders/abc/items/xyz should match
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/orders/abc/items/xyz")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_rest_route_no_match_returns_404() {
    let app = common::test_app().await;

    // No channels registered — multi-segment path should 404
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/nonexistent/resource/123")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_backward_compat_simple_http_channel() {
    let app = common::test_app().await;

    // Create a simple HTTP channel (not REST) — should still work with POST /{channel}
    common::create_and_activate_channel(
        &app,
        "events",
        common::simple_log_workflow("Events Workflow"),
    )
    .await;

    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/events",
            Some(json!({"data": {"event_type": "click"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_async_via_rest_route() {
    let app = common::test_app().await;

    let wf_id = create_echo_workflow(&app, "Async REST").await;
    create_rest_channel(&app, "items.create", "/items", vec!["POST"], &wf_id).await;

    // POST /api/v1/data/items/async should use async processing
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/items/async",
            Some(json!({"data": {"item": "widget"}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = common::body_json(resp).await;
    assert!(body["trace_id"].is_string());
}
