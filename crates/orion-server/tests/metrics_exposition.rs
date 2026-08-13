//! End-to-end `/metrics` exposition (T37).
//!
//! The integration binary can never assert the rendered exposition: the
//! metrics recorder is process-global, so `common` gives the first app the
//! real `PrometheusHandle` and every later app a no-op — which test wins is
//! a race, so `metrics_endpoint_test` checks status codes only. This binary
//! exists to *be* the first app deterministically: one test, one process,
//! the real recorder — and the rendered body finally gets asserted, not just
//! the 200.

#[path = "integration/common/mod.rs"]
mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn rendered_exposition_carries_the_advertised_families() {
    // metrics.enabled defaults to off; the route is only registered when on.
    let mut config = orion::config::AppConfig::default();
    config.metrics.enabled = true;
    let app = common::test_app_with_config(config).await;

    // Drive one request through the data plane so the request-path families
    // have something to count. 404 is fine — it still traverses the HTTP
    // metrics middleware, which is the layer under test.
    let _ = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/no-such-channel",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("data request");

    let resp = app
        .clone()
        .oneshot(common::json_request("GET", "/metrics", None))
        .await
        .expect("metrics request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let text = String::from_utf8(body.to_vec()).expect("exposition is UTF-8");

    assert!(
        !text.trim().is_empty(),
        "the rendered exposition must not be empty — an empty body means this \
         binary no longer owns the real recorder"
    );
    // One gauge set at startup, one family driven by the request above: both
    // ends of the pipeline (process state and request path) render.
    for family in ["orion_active_workflows", "orion_http_requests_total"] {
        assert!(
            text.contains(family),
            "family `{family}` missing from the exposition:\n{text}"
        );
    }
}
