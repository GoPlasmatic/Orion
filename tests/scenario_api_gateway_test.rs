mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

/// Helper: create a workflow, activate it, and return its workflow_id.
async fn create_and_activate_workflow(
    app: &axum::Router,
    workflow_json: serde_json::Value,
) -> String {
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow_json),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{}/status", wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wf_id
}

/// Helper: create a REST channel with the given route pattern and methods, activate it,
/// and return the channel_id.
async fn create_rest_channel(
    app: &axum::Router,
    name: &str,
    route_pattern: &str,
    methods: Vec<&str>,
    workflow_id: &str,
) -> String {
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
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{}/status", ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    ch_id
}

// ---------------------------------------------------------------------------
// Test 1: API gateway extracts path params and forwards to an upstream service
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gateway_path_param_forwarding() {
    // -- Mock upstream HTTP server returning user data keyed by id --------
    let mock_app = axum::Router::new().route(
        "/api/users/{id}",
        axum::routing::get(
            |axum::extract::Path(id): axum::extract::Path<String>| async move {
                axum::Json(json!({"id": id, "name": "Alice"}))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    // -- Orion app --------------------------------------------------------
    let app = common::test_app().await;

    // Create HTTP connector pointing at the mock upstream
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/connectors",
            Some(json!({
                "id": "upstream-api",
                "name": "upstream-api",
                "connector_type": "http",
                "config": {
                    "type": "http",
                    "url": format!("http://{}", addr),
                    "retry": {"max_retries": 0, "retry_delay_ms": 10},
                    "allow_private_urls": true
                }
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Workflow:
    //   1. map: extract user_id from metadata.params into data.user_id
    //   2. http_call: GET /api/users/<user_id> via path_logic
    //   3. map: copy upstream response fields to top-level data for easy assertion
    let wf_id = create_and_activate_workflow(
        &app,
        common::workflow_with_tasks(
            "User Lookup Gateway",
            json!([
                {
                    "id": "t1",
                    "name": "Extract path param",
                    "function": {
                        "name": "map",
                        "input": {
                            "mappings": [{
                                "path": "data.user_id",
                                "logic": {"var": "metadata.params.user_id"}
                            }]
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Call upstream",
                    "function": {
                        "name": "http_call",
                        "input": {
                            "connector": "upstream-api",
                            "method": "GET",
                            "path_logic": {"cat": ["/api/users/", {"var": "data.user_id"}]},
                            "response_path": "data.upstream",
                            "timeout_ms": 5000
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    // REST channel: GET /users/{user_id}
    create_rest_channel(&app, "user-lookup", "/users/{user_id}", vec!["GET"], &wf_id).await;

    // -- Send request through the gateway ---------------------------------
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/users/42")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    // The upstream mock returns {"id": "42", "name": "Alice"}
    assert_eq!(body["data"]["upstream"]["id"], "42");
    assert_eq!(body["data"]["upstream"]["name"], "Alice");
}

// ---------------------------------------------------------------------------
// Test 2: Same path pattern, different HTTP methods routed to different channels
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gateway_multi_method_routing() {
    let app = common::test_app().await;

    // -- GET channel: read an item ----------------------------------------
    let get_wf_id = create_and_activate_workflow(
        &app,
        common::workflow_with_tasks(
            "Items Read Workflow",
            json!([{
                "id": "t1",
                "name": "Mark as read",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            {"path": "data.action", "logic": "read"},
                            {"path": "data.item_id", "logic": {"var": "metadata.params.id"}}
                        ]
                    }
                }
            }]),
        ),
    )
    .await;

    create_rest_channel(&app, "items-get", "/items/{id}", vec!["GET"], &get_wf_id).await;

    // -- POST channel: create an item -------------------------------------
    let post_wf_id = create_and_activate_workflow(
        &app,
        common::workflow_with_tasks(
            "Items Create Workflow",
            json!([
                {
                    "id": "t1",
                    "name": "Parse body",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "input"}
                    }
                },
                {
                    "id": "t2",
                    "name": "Mark as create",
                    "function": {
                        "name": "map",
                        "input": {
                            "mappings": [
                                {"path": "data.action", "logic": "create"},
                                {"path": "data.item_id", "logic": {"var": "metadata.params.id"}}
                            ]
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    create_rest_channel(&app, "items-post", "/items/{id}", vec!["POST"], &post_wf_id).await;

    // -- Request 1: GET /api/v1/data/items/123 ----------------------------
    let get_req = Request::builder()
        .method("GET")
        .uri("/api/v1/data/items/123")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(get_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["action"], "read");
    assert_eq!(body["data"]["item_id"], "123");

    // -- Request 2: POST /api/v1/data/items/123 ---------------------------
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/items/123",
            Some(json!({"data": {"name": "Widget"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["action"], "create");
    assert_eq!(body["data"]["item_id"], "123");
}
