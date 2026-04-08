mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{
    body_json, create_and_activate_channel_with_config, post_with_idempotency_key,
    simple_log_workflow,
};

// ============================================================
// 1. Duplicate key within the window is rejected with 409
// ============================================================

#[tokio::test]
async fn test_dedup_rejects_duplicate() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "dedup-ch",
        simple_log_workflow("Dedup WF"),
        json!({
            "deduplication": {
                "header": "Idempotency-Key",
                "window_secs": 300
            }
        }),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // First request with key-001 — should succeed
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-ch",
            "key-001",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request with same key-001 — should be rejected as duplicate
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-ch",
            "key-001",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Duplicate"),
        "Expected error message to contain 'Duplicate', got: {}",
        body["error"]["message"]
    );

    // Third request with a different key-002 — should succeed
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-ch",
            "key-002",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// 2. Duplicate key is allowed after the dedup window expires
// ============================================================

#[tokio::test]
async fn test_dedup_allows_after_window() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "dedup-expire-ch",
        simple_log_workflow("Dedup Expire WF"),
        json!({
            "deduplication": {
                "header": "Idempotency-Key",
                "window_secs": 1
            }
        }),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // First request — should succeed
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-expire-ch",
            "expire-key",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wait for the 1-second dedup window to expire
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Same key after expiry — should succeed (window expired)
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-expire-ch",
            "expire-key",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// 3. Missing idempotency header passes through (no dedup check)
// ============================================================

#[tokio::test]
async fn test_dedup_missing_header_passes() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "dedup-noheader-ch",
        simple_log_workflow("Dedup NoHeader WF"),
        json!({
            "deduplication": {
                "header": "Idempotency-Key",
                "window_secs": 300
            }
        }),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // First request WITHOUT the Idempotency-Key header — should pass
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/dedup-noheader-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request also without the header — should also pass (no key to dedup on)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/dedup-noheader-ch",
            Some(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// 4. Different idempotency keys both pass
// ============================================================

#[tokio::test]
async fn test_dedup_different_keys_both_pass() {
    let app = common::test_app().await;

    create_and_activate_channel_with_config(
        &app,
        "dedup-diff-ch",
        simple_log_workflow("Dedup Diff WF"),
        json!({
            "deduplication": {
                "header": "Idempotency-Key",
                "window_secs": 300
            }
        }),
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // Request with key "aaa" — should succeed
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-diff-ch",
            "aaa",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Request with key "bbb" — should also succeed (different key)
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-diff-ch",
            "bbb",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
