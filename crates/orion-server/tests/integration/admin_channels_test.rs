use crate::common;

use axum::http::StatusCode;
use tower::ServiceExt;

/// Archiving a channel must stop the data plane serving it — found by the
/// cluster archive-propagation test: the single-segment name fallback in the
/// dynamic data handler accepted ANY name and ran the engine against an
/// empty workflow set, returning 200 "ok" for archived channels.
#[tokio::test]
async fn archived_channel_is_not_served() {
    let app = common::test_app().await;
    let (channel_id, _wf) = common::create_and_activate_channel_full(
        &app,
        "arch-stop-ch",
        common::simple_log_workflow("Arch Stop WF"),
        serde_json::json!({}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/arch-stop-ch",
            Some(serde_json::json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "active channel must serve");

    // Archiving is a deployment without it.
    common::remove_member(&app, "channel", &channel_id).await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/arch-stop-ch",
            Some(serde_json::json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "archived channel must not serve, got {status}: {body}"
    );
}

/// A channel name that never existed must 404 on both the sync and async
/// data planes — not silently execute an empty workflow set.
#[tokio::test]
async fn unknown_channel_name_returns_404() {
    let app = common::test_app().await;
    for uri in [
        "/api/v2/data/never-created-ch",
        "/api/v2/data/never-created-ch/async",
    ] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                uri,
                Some(serde_json::json!({"data": {"x": 1}})),
            ))
            .await
            .unwrap();
        let status = resp.status();
        let body = common::body_json(resp).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri}: unknown channel must 404, got {status}: {body}"
        );
        assert_eq!(body["error"]["code"], "NOT_FOUND", "{uri}: {body}");
    }
}
