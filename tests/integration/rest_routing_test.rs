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

// ============================================================
// N10: byte-exact matching + percent-decoded params
// ============================================================

/// N10: RFC 3986 makes the path component case-sensitive; the old
/// `eq_ignore_ascii_case` match let `/ORDERS/1` resolve to `/orders/{id}`
/// while everything keyed off the raw path (response-cache keys included)
/// treated the two spellings as different requests.
#[tokio::test]
async fn route_matching_is_case_sensitive() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    create_rest_channel(&app, "orders.get", "/orders/{id}", vec!["GET"], &wf_id).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/data/ORDERS/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "/ORDERS/1 must not match /orders/{{id}}"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/data/orders/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// N10: captured params are percent-decoded exactly once at extraction, so
/// `%2F` expresses a literal `/` inside a parameter — previously the
/// workflow received the raw escape and no author decoded it.
#[tokio::test]
async fn route_params_arrive_percent_decoded() {
    let app = common::test_app().await;
    let wf_id = create_and_activate_workflow(
        &app,
        common::workflow_with_tasks(
            "Param Echo",
            json!([{
                "id": "echo-param",
                "name": "Echo param",
                "function": {
                    "name": "map",
                    "input": {"mappings": [
                        {"path": "data.param_id", "logic": {"var": "metadata.params.id"}}
                    ]}
                }
            }]),
        ),
    )
    .await;
    create_rest_channel(&app, "orders.param", "/orders/{id}", vec!["GET"], &wf_id).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/data/orders/a%2Fb")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(
        body["data"]["param_id"], "a/b",
        "params must arrive decoded exactly once: {body}"
    );
}

/// N10: an invalid percent-sequence is a malformed request path — 400, not
/// a silent literal match, and not a fall-through to some other resolution.
#[tokio::test]
async fn invalid_percent_sequence_returns_400() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    create_rest_channel(&app, "orders.get", "/orders/{id}", vec!["GET"], &wf_id).await;

    for uri in [
        "/api/v1/data/orders/a%ZZ",
        "/api/v1/data/orders/trailing%2",
        "/api/v1/data/bad%GGname",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{uri} must be rejected as malformed"
        );
    }
}

// ============================================================
// R6: route patterns and methods are structurally validated
// ============================================================

/// Create a REST channel without activating it, returning the status and body.
async fn try_create_rest_channel(
    app: &axum::Router,
    name: &str,
    route_pattern: &str,
    methods: Vec<&str>,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": name,
                "channel_type": "sync",
                "protocol": "rest",
                "methods": methods,
                "route_pattern": route_pattern,
            })),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, common::body_json(resp).await)
}

/// R6: `methods: ["POTS"]` and `route_pattern: "orders/{id"` were created,
/// activated and reloaded — and then never matched a request, with nothing
/// reporting it. The channel was simply dead.
#[tokio::test]
async fn a_channel_that_could_never_match_is_rejected_at_create() {
    let app = common::test_app().await;

    let (status, body) = try_create_rest_channel(&app, "typo-verb", "/orders", vec!["POTS"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("POTS"), "{body}");

    let (status, body) =
        try_create_rest_channel(&app, "unbalanced", "/orders/{id", vec!["GET"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("brace"), "{body}");

    let (status, body) = try_create_rest_channel(&app, "no-slash", "orders", vec!["GET"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("must start with"), "{body}");

    let (status, body) =
        try_create_rest_channel(&app, "dup-param", "/o/{id}/i/{id}", vec!["GET"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("more than once"), "{body}");

    // The well-formed equivalent still creates.
    let (status, body) =
        try_create_rest_channel(&app, "fine", "/orders/{id}", vec!["GET", "post"]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Both problems are reported in one response, not one round-trip each — the
/// B1 shape the protocol-required checks already used.
#[tokio::test]
async fn every_route_problem_is_reported_at_once() {
    let app = common::test_app().await;
    let (status, body) =
        try_create_rest_channel(&app, "two-problems", "/orders/{2id}", vec!["POTS"]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let details = body["error"]["details"].as_array().expect("details");
    assert_eq!(details.len(), 2, "{body}");
    let paths: Vec<&str> = details.iter().filter_map(|d| d["path"].as_str()).collect();
    assert!(paths.contains(&"channel.methods"), "{body}");
    assert!(paths.contains(&"channel.route_pattern"), "{body}");
}

// ============================================================
// R7: two active channels cannot claim one route
// ============================================================

/// R7: `RouteTable::match_route` returns the first hit, so a second claimant
/// is dead — requests to its declared path run the incumbent's workflow. The
/// tie used to break on DB row order, so which one served could differ per node
/// and change on any reload.
#[tokio::test]
async fn a_second_channel_cannot_claim_a_route_another_already_serves() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    create_rest_channel(&app, "orders.first", "/orders/{id}", vec!["GET"], &wf_id).await;

    // Same shape, different parameter name — the name is a workflow-facing
    // label, not part of the match, so these collide.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "orders.second",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["GET"],
                "route_pattern": "/orders/{order_id}",
                "workflow_id": wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "create stays permissive"
    );
    let ch_id = common::body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{ch_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = common::body_json(resp).await.to_string();
    assert!(
        msg.contains("orders.first"),
        "must name the incumbent: {msg}"
    );
    assert!(msg.contains("/orders/{}"), "must name the route: {msg}");

    // The incumbent keeps serving — activating a second channel must never
    // take a running one down.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/data/orders/123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A collision needs the method sets to actually overlap: `GET /orders/{id}`
/// and `DELETE /orders/{id}` are two different routes.
#[tokio::test]
async fn disjoint_methods_on_one_path_are_not_a_conflict() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    create_rest_channel(&app, "orders.get", "/orders/{id}", vec!["GET"], &wf_id).await;
    // Activates cleanly: no request can match both.
    create_rest_channel(&app, "orders.del", "/orders/{id}", vec!["DELETE"], &wf_id).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/data/orders/9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A new *version* of the same channel replaces itself rather than colliding
/// with itself — otherwise no REST channel could ever be edited.
#[tokio::test]
async fn a_new_version_of_the_same_channel_does_not_collide() {
    let app = common::test_app().await;
    let wf_id = create_echo_workflow(&app, "Orders Workflow").await;
    let ch_id = create_rest_channel(&app, "orders.v", "/orders/{id}", vec!["GET"], &wf_id).await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/admin/channels/{ch_id}/versions"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{ch_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.status());
}
