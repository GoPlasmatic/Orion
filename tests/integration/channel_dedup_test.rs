use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{
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
                // 300s + pause–advance–resume instead of a 1s window and a
                // real sleep: expiry runs on tokio's clock, and the large
                // window keeps timer auto-advance from crossing it early.
                "window_secs": 300
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

    // Advance the paused clock past the dedup window — no wall time burned.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::time::resume();

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

// ============================================================
// 5. Dedup keys are scoped per channel — the same idempotency
//    key on two channels sharing a backend must not collide
// ============================================================

#[tokio::test]
async fn test_dedup_key_scoped_per_channel() {
    let app = common::test_app().await;

    let dedup_config = json!({
        "deduplication": {
            "header": "Idempotency-Key",
            "window_secs": 300
        }
    });
    create_and_activate_channel_with_config(
        &app,
        "dedup-scope-a",
        simple_log_workflow("Dedup Scope A WF"),
        dedup_config.clone(),
    )
    .await;
    create_and_activate_channel_with_config(
        &app,
        "dedup-scope-b",
        simple_log_workflow("Dedup Scope B WF"),
        dedup_config,
    )
    .await;

    let payload = json!({"data": {"k": "v"}});

    // Same key on channel A — succeeds
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-scope-a",
            "shared-token",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Same key on channel B — must also succeed (was 409 before channel scoping)
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-scope-b",
            "shared-token",
            payload.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Replay on channel A — still deduplicated within its own scope
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/dedup-scope-a",
            "shared-token",
            payload,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
