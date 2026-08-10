//! K14: package receipts — the applied-version immutability gate.
//!
//! The rule under test: a `staged` receipt may be re-put with new content
//! (only a draft can be updated); an `applied` receipt is immutable — the
//! same version with a different hash is a 409, and going back to `staged`
//! is refused. Re-putting an *older* applied version with its own original
//! hash is legal and makes it current again (the rollback path).

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn put_receipt(
    app: &axum::Router,
    name: &str,
    version: &str,
    hash: &str,
    state: &str,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/packages/{name}"),
            Some(json!({"version": version, "content_hash": hash, "state": state})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn staged_receipt_updates_in_place_and_applies() {
    let app = test_app().await;

    let (status, body) = put_receipt(&app, "payments", "1.0.0", "sha256:aaa", "staged").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["state"], "staged");

    // Only a draft can be updated: same version, new content, still staged.
    let (status, body) = put_receipt(&app, "payments", "1.0.0", "sha256:bbb", "staged").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["content_hash"], "sha256:bbb");

    // Flip to applied.
    let (status, body) = put_receipt(&app, "payments", "1.0.0", "sha256:bbb", "applied").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["state"], "applied");

    // The detail view names it current.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages/payments", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["current"]["version"], "1.0.0");
    assert_eq!(body["data"]["versions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn applied_version_is_immutable() {
    let app = test_app().await;
    let (status, _) = put_receipt(&app, "billing", "2.0.0", "sha256:aaa", "applied").await;
    assert_eq!(status, StatusCode::OK);

    // Same version + different content → 409, with the bump instruction.
    let (status, body) = put_receipt(&app, "billing", "2.0.0", "sha256:bbb", "applied").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bump the package version"),
        "{body}"
    );

    // Same version + same content → idempotent re-apply, not a conflict.
    let (status, body) = put_receipt(&app, "billing", "2.0.0", "sha256:aaa", "applied").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Applied never demotes to staged, even content-identical.
    let (status, body) = put_receipt(&app, "billing", "2.0.0", "sha256:aaa", "staged").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn reapplying_an_older_version_is_the_rollback_path() {
    let app = test_app().await;
    let (s, _) = put_receipt(&app, "orders", "1.0.0", "sha256:v1", "applied").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = put_receipt(&app, "orders", "1.1.0", "sha256:v2", "applied").await;
    assert_eq!(s, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages/orders", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["current"]["version"], "1.1.0");

    // SQLite timestamps are second-granular; the touch must land strictly
    // later than 1.1.0's updated_at for `current` to move back.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (s, body) = put_receipt(&app, "orders", "1.0.0", "sha256:v1", "applied").await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages/orders", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["current"]["version"], "1.0.0",
        "re-applying an older receipted version must make it current again: {body}"
    );
    assert_eq!(body["data"]["versions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_pages_and_missing_package_404s() {
    let app = test_app().await;
    for v in ["1.0.0", "1.1.0"] {
        let (s, _) = put_receipt(&app, "pkg-a", v, "sha256:x", "staged").await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _) = put_receipt(&app, "pkg-b", "0.1.0", "sha256:y", "applied").await;
    assert_eq!(s, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages?limit=2", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 3);
    assert_eq!(body["data"].as_array().unwrap().len(), 2);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages/absent", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn receipt_fields_are_validated() {
    let app = test_app().await;

    // Empty version.
    let (s, body) = put_receipt(&app, "pkg", "", "sha256:x", "staged").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // Charset: version with a space.
    let (s, body) = put_receipt(&app, "pkg", "1.0 beta", "sha256:x", "staged").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // Unknown state is a per-request parse error.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/packages/pkg",
            Some(json!({"version": "1.0.0", "content_hash": "sha256:x", "state": "deployed"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Over-long name (the MySQL column caps are enforced at the route).
    let long = "n".repeat(129);
    let (s, body) = put_receipt(&app, &long, "1.0.0", "sha256:x", "staged").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
}

/// The PUT lands on the audit trail with the package-qualified resource id.
#[tokio::test]
async fn receipt_puts_are_audited() {
    let app = test_app().await;
    let (s, _) = put_receipt(&app, "audited-pkg", "1.0.0", "sha256:x", "applied").await;
    assert_eq!(s, StatusCode::OK);

    let body = crate::common::wait_for_body(
        &app,
        "/api/v1/admin/audit-logs?resource_type=package",
        |body| body["data"].as_array().is_some_and(|rows| !rows.is_empty()),
    )
    .await;
    let row = &body["data"][0];
    assert_eq!(row["action"], "package_applied");
    assert_eq!(row["resource_id"], "audited-pkg@1.0.0");
}
