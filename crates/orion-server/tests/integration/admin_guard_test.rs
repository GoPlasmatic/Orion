//! What the router guards is what the OpenAPI document says it guards.
//!
//! Admin authentication is attached to route groups when the router is built
//! (`routes::api_routes`), and the document's `security` blocks come from
//! `admin_auth::is_guarded_path`. Two statements of one decision: this asks
//! every documented operation, anonymously, and holds each to its document —
//! a guarded one must answer 401; an open one may only when it documents a 401
//! of its own (the single-trace read, which authenticates itself).

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common;

#[tokio::test]
async fn every_documented_operation_is_guarded_exactly_when_documented() {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec!["a-sufficiently-long-test-secret-key-000".to_string()];
    // Served on this listener, so its documented guard is reachable.
    config.metrics.enabled = true;
    let app = common::test_app_with_config(config).await;

    let spec = common::body_json(
        app.clone()
            .oneshot(common::json_request("GET", "/api/v2/openapi.json", None))
            .await
            .unwrap(),
    )
    .await;
    let mut checked = 0;
    for (template, item) in spec["paths"].as_object().unwrap() {
        // A path parameter becomes a value no route or row has, so a handler
        // that runs answers 404 rather than anything resembling a 401.
        let path = template
            .split('/')
            .map(|seg| {
                if seg.starts_with('{') {
                    "00000000-0000-0000-0000-000000000000"
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        for (method, operation) in item.as_object().unwrap() {
            let Ok(method) = method.to_uppercase().parse::<axum::http::Method>() else {
                continue;
            };
            let documented = operation
                .get("security")
                .and_then(|s| s.as_array())
                .is_some_and(|s| !s.is_empty());
            let resp = app
                .clone()
                .oneshot(common::json_request(method.as_str(), &path, None))
                .await
                .unwrap();
            let refused = resp.status() == StatusCode::UNAUTHORIZED;
            let self_authenticating = operation["responses"].get("401").is_some();
            assert!(
                if documented {
                    refused
                } else {
                    !refused || self_authenticating
                },
                "{method} {template}: answered {} to an anonymous caller, documented as {}",
                resp.status(),
                if documented { "guarded" } else { "open" }
            );
            checked += 1;
        }
    }
    assert!(checked > 40, "only {checked} operations were checked");
}
