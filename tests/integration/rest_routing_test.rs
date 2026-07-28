use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{create_and_activate_workflow, create_rest_channel};

/// Create and activate the shared echo workflow (echoes input + adds a
/// `data.matched` marker), returning its workflow_id.
async fn create_echo_workflow(app: &axum::Router, name: &str) -> String {
    create_and_activate_workflow(app, common::echo_workflow(name)).await
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
    // GET doesn't match the POST-only route, and the fallback name lookup
    // ("orders" is a route pattern, not a channel name) finds nothing — the
    // request must be refused, not silently run against an empty workflow
    // set as it once was.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND", "{body}");
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

/// R8: `traces` is an ordinary channel name again.
///
/// `/traces` and `/traces/{id}` used to be static routes on the data plane.
/// Axum resolves static segments before `/{*path}`, so a channel called
/// `traces` was permanently unreachable — `POST /api/v1/data/traces` hit the
/// GET-only trace list and returned 405, with no reserved-name check anywhere
/// to explain why. The endpoints moved to `/api/v1/admin/traces*` in 1.0,
/// where the admin-guarded list always belonged, leaving the data plane a
/// single catch-all.
#[tokio::test]
async fn a_channel_named_traces_is_reachable() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "traces",
        json!({
            "name": "Traces Channel",
            "condition": true,
            "tasks": [{
                "id": "mark",
                "name": "Mark",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.ok", "logic": {"cat": ["y", "es"]}}]}
                }
            }]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/traces",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a channel named `traces` must serve like any other, not 405"
    );
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["ok"], "yes");
}

/// F39: `RouteTable::build` filtered to `channel_type == "sync"`, but channel
/// validation *requires* a `route_pattern` for REST/HTTP protocols regardless
/// of type. So an async REST channel was forced to declare a route that was
/// then silently ignored — the channel was reachable by name and its declared
/// route 404'd forever, with a 201 at create and a clean activation.
#[tokio::test]
async fn an_async_rest_channel_is_reachable_at_its_route_pattern() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Async Route Workflow").await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "async.orders",
                "channel_type": "async",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/async-orders/{id}",
                "workflow_id": wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{id}/status"),
            Some(json!({ "status": "active" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The declared route, submitted asynchronously.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/async-orders/42/async",
            Some(json!({ "data": { "x": 1 } })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "an async channel's route_pattern must resolve: {}",
        common::body_json(resp).await
    );
}
