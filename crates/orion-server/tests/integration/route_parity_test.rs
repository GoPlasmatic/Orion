//! M-18: every path the client builds must resolve on the server.
//!
//! `orion-client::paths` is the one place `/api/v2/admin/...` strings live,
//! and a rename on either side used to surface only in the CLI at run time.
//! This sends each operational path through the real router and asserts the
//! answer is not the router's own 404/405 fallback. A handler is free to say
//! 400, 401, 500 — that proves the route exists.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use orion_client::paths;

use crate::common;

#[tokio::test]
async fn every_operational_client_path_resolves_on_the_server() {
    let app = common::test_app().await;
    let cases: &[(Method, &str)] = &[
        (Method::GET, paths::TRACES),
        (Method::GET, paths::TRACE_DLQ),
        (Method::POST, paths::TRACE_DLQ_PURGE),
        (Method::GET, paths::CRON_OCCURRENCES),
        (Method::GET, paths::CRON_STATUS),
        (Method::GET, paths::AUDIT_LOGS),
        (Method::GET, paths::BACKUPS),
        (Method::GET, paths::ENGINE_STATUS),
        (Method::GET, paths::FUNCTIONS),
        (Method::GET, paths::V2_RUNTIME_CAPABILITIES),
        (Method::GET, paths::V2_CONNECTOR_PACKAGES),
        (Method::GET, paths::V2_CONNECTIONS),
        (Method::POST, paths::V2_DEPLOYMENT_PLANS),
        (Method::POST, paths::V2_DEFINITIONS),
        (Method::GET, paths::V2_DEPLOYMENTS),
        (Method::GET, paths::HEALTH),
    ];

    for (method, path) in cases {
        let request = Request::builder()
            .method(method.clone())
            .uri(*path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("router answer");
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} is not registered on the router"
        );
        assert_ne!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} is registered under another method"
        );
    }
}
