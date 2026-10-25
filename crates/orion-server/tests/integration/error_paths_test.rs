use crate::common;

use crate::common::json_request;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Content type & body validation
// ============================================================

#[tokio::test]
async fn test_non_json_content_type_rejected_with_415() {
    let app = common::test_app().await;

    // Non-JSON Content-Type with a body should return 415 Unsupported Media Type.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v2/data/orders")
        .header("content-type", "text/plain")
        .body(Body::from(r#"{"data": {"key": "value"}}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_completely_invalid_json_body() {
    let app = common::test_app().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v2/data/orders")
        .header("content-type", "application/json")
        .body(Body::from("this is not json {{{"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn test_empty_body_accepted() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "empty-body-ch",
        common::simple_log_workflow("Empty Body WF"),
    )
    .await;

    // Empty body is treated as {data: {}, metadata: {}} — valid for GET/DELETE
    // or any request without payload.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v2/data/empty-body-ch")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_success());
}

/// R13: this endpoint had three behaviours for three body shapes, and only
/// one was documented. `{"data": …}` was the envelope, an empty body became
/// `{"data":{}}`, and a **bare object** — the obvious thing to send, and what
/// every other JSON API accepts — failed with *missing field `data`*. On the
/// most-hit endpoint in the product.
///
/// One rule now: an object carrying `data` or `metadata` is the envelope;
/// anything else is the payload.
#[tokio::test]
async fn a_bare_object_body_is_the_payload() {
    let app = common::test_app().await;
    let wf_id = common::create_and_activate_workflow(&app, common::echo_workflow("Echo")).await;
    common::create_rest_channel(&app, "bare", "/bare", vec!["POST"], &wf_id).await;

    // Bare object: previously a 400.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/bare",
            Some(json!({"amount": 5})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bare = common::body_json(resp).await;

    // …and it means exactly what the explicit envelope means.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/bare",
            Some(json!({"data": {"amount": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let enveloped = common::body_json(resp).await;
    assert_eq!(bare["data"], enveloped["data"], "{bare} vs {enveloped}");

    // A non-object body is a payload too.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/bare",
            Some(json!([1, 2, 3])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// An envelope naming only `metadata` carries no payload — which is `{}`, the
/// same thing an empty body means.
#[tokio::test]
async fn an_envelope_with_only_metadata_is_accepted() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "meta-only",
        common::simple_log_workflow("Meta Only"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/meta-only",
            Some(json!({"metadata": {"source": "test"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// 404 on nonexistent resources
// ============================================================

#[tokio::test]
async fn test_nonexistent_trace_returns_404() {
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces/nonexistent-trace-id",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// Router fallbacks — every non-2xx uses the error envelope (R9)
// ============================================================

#[tokio::test]
async fn unmatched_path_returns_the_error_envelope() {
    // Previously a zero-length body, which broke any client that parses the
    // body on error and contradicted the documented contract.
    let app = common::test_app().await;

    let resp = app
        .oneshot(json_request("GET", "/no/such/path", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers().contains_key("x-request-id"),
        "the 404 must be correlatable"
    );
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn method_mismatch_returns_405_with_the_error_envelope() {
    let app = common::test_app().await;

    // /healthz is GET-only.
    let resp = app
        .oneshot(json_request("DELETE", "/healthz", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "METHOD_NOT_ALLOWED");
}

// ============================================================
// R16: the admin body limit is separate from the data-plane one
// ============================================================

/// R16: the body limit was one global layer set from `ingest.max_payload_size`
/// — a name that says *data plane* — so admin uploads shared a ceiling with
/// anonymous channel traffic. Raising it for a big definition raised it for
/// the unauthenticated plane too, which is the opposite of what an operator
/// wants. `POST /definitions` answers under `server.max_admin_body_size`.
#[tokio::test]
async fn the_admin_body_limit_is_independent_of_the_data_plane_one() {
    use orion::config::AppConfig;

    let mut config = AppConfig::default();
    // A data plane locked down tight, and an admin API that can still take an
    // import. Under one shared limit this pair is unexpressible.
    config.ingest.max_payload_size = 512;
    config.server.max_admin_body_size = 256 * 1024;

    let app = common::test_app_with_config(config).await;
    common::create_and_activate_channel(&app, "tiny", common::simple_log_workflow("Tiny")).await;

    let big_payload = json!({ "data": { "blob": "x".repeat(4096) } });

    // Data plane: over its 512-byte bound.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/tiny",
            Some(big_payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the data plane must still honour ingest.max_payload_size"
    );
    // ...and say so in the envelope. This used to be the one non-2xx in the
    // whole surface that answered with axum's plain-text rejection instead,
    // so a client's error path could not parse the only field it branches on.
    let body = common::body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "PAYLOAD_TOO_LARGE",
        "an oversize body must carry the error envelope like every other refusal: {body}"
    );

    // Admin: the same bytes, well under its own bound.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({"kind": "workflow", "definition": {
                "name": "Big Description",
                "description": "y".repeat(2000),
                "tasks": [{"id":"t1","name":"Log",
                           "function":{"name":"log","input":{"message":"x"}}}]
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "an admin body over the data-plane limit must still be accepted"
    );

    // Admin, over its *own* bound: the same condition as the data plane's, so
    // the same status. (`POST /definitions` carries plugin components, so its
    // bound is the component-sized one; a connection revision is held to
    // `server.max_admin_body_size`.)
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/connections",
            Some(json!({"connection_id": "big", "revision": 1,
                        "provider_digest": "sha256:x",
                        "public_config": { "note": "y".repeat(300 * 1024) }})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "both planes must answer an oversize body the same way"
    );

    // A definition upload is bounded by the largest plugin component it can
    // carry, not by `server.max_admin_body_size`: the same 300 KiB reaches
    // validation, which refuses it for what it is.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({"kind": "workflow", "definition": { "name": "y".repeat(300 * 1024) }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
