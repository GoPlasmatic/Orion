//! F8: `http_call` must not replay non-idempotent side effects.
//!
//! `max_retries` defaults to 3 and the retry loop used to run regardless of
//! method, so a POST that timed out (or 5xx'd) was re-sent up to 3× with no
//! idempotency key — duplicate charges and duplicate orders out of the box.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, json_request};

/// Mock server that counts requests per method and always fails, so every
/// retry the client makes is visible.
async fn start_counting_server() -> (std::net::SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let posts = Arc::new(AtomicUsize::new(0));
    let gets = Arc::new(AtomicUsize::new(0));
    let post_counter = posts.clone();
    let get_counter = gets.clone();

    let mock_app = axum::Router::new()
        .route(
            "/thing",
            axum::routing::post(move || {
                let c = post_counter.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "boom"})),
                    )
                }
            }),
        )
        .route(
            "/thing-get",
            axum::routing::get(move || {
                let c = get_counter.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "boom"})),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });
    (addr, posts, gets)
}

async fn create_connector(
    app: &axum::Router,
    name: &str,
    addr: std::net::SocketAddr,
    retry_non_idempotent: bool,
) {
    common::create_connector(
        app,
        json!({
            "id": name,
            "name": name,
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{}", addr),
                "retry": {"max_retries": 2, "retry_delay_ms": 10},
                "retry_non_idempotent": retry_non_idempotent,
                "allow_private_urls": true
            }
        }),
    )
    .await;
}

fn call_workflow(name: &str, connector: &str, method: &str, path: &str) -> serde_json::Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "t1",
            "name": "Call endpoint",
            "continue_on_error": true,
            "function": {
                "name": "http_call",
                "input": {
                    "connector": connector,
                    "method": method,
                    "path": path,
                    "body": {"amount": 100},
                    "response_path": "data.result",
                    "timeout_ms": 2000
                }
            }
        }]),
    )
}

#[tokio::test]
async fn post_is_not_retried_by_default() {
    let app = common::test_app().await;
    let (addr, posts, _gets) = start_counting_server().await;
    create_connector(&app, "f8-post", addr, false).await;
    common::create_and_activate_channel(
        &app,
        "f8-post-ch",
        call_workflow("F8 Post", "f8-post", "POST", "/thing"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f8-post-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK); // continue_on_error

    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "a POST must be sent exactly once — retrying replays the side effect (F8)"
    );
}

#[tokio::test]
async fn get_still_retries() {
    let app = common::test_app().await;
    let (addr, _posts, gets) = start_counting_server().await;
    create_connector(&app, "f8-get", addr, false).await;
    common::create_and_activate_channel(
        &app,
        "f8-get-ch",
        call_workflow("F8 Get", "f8-get", "GET", "/thing-get"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f8-get-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        gets.load(Ordering::SeqCst),
        3,
        "idempotent methods keep retrying: 1 attempt + 2 retries"
    );
}

#[tokio::test]
async fn post_retries_when_connector_opts_in() {
    let app = common::test_app().await;
    let (addr, posts, _gets) = start_counting_server().await;
    create_connector(&app, "f8-post-optin", addr, true).await;
    common::create_and_activate_channel(
        &app,
        "f8-post-optin-ch",
        call_workflow("F8 Post Opt-In", "f8-post-optin", "POST", "/thing"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f8-post-optin-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        posts.load(Ordering::SeqCst),
        3,
        "retry_non_idempotent = true restores the old behaviour"
    );
}
