//! `/readyz` component reporting (D15).
//!
//! The cluster-Redis probe only appears when cluster mode is on — a
//! single-node deployment has no shared Redis to be unready about. The
//! negative case (Redis unreachable → 503) needs a real container and lives
//! in `tests/cluster`.

use axum::http::StatusCode;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

#[tokio::test]
async fn readyz_omits_cluster_redis_on_a_single_node() {
    let app = common::test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/readyz", None))
        .await
        .expect("readyz");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["components"]["database"], "ok");
    assert_eq!(body["components"]["engine"], "ok");
    assert!(
        body["components"].get("cluster_redis").is_none(),
        "cluster_redis must be absent outside cluster mode: {body}"
    );
}
