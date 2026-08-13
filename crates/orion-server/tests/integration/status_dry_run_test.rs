//! K3: `?dry_run=true` on the two status endpoints runs every activation
//! (or archive) gate without writing, answering the `/validate` envelope.
//! K8: channel activation refuses a workflow that is missing or not active —
//! the documented gate that used to surface only as a reload-time quarantine.

use crate::common::{self, body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn patch_status(app: &axum::Router, uri: &str, status: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("PATCH", uri, Some(json!({"status": status}))))
        .await
        .unwrap();
    let code = resp.status();
    (code, body_json(resp).await)
}

#[tokio::test]
async fn workflow_activation_dry_run_reports_without_writing() {
    let app = test_app().await;
    let id =
        common::create_and_return_workflow_id(&app, common::simple_log_workflow("Dry Run Green"))
            .await;

    // Green: a clean draft would activate.
    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/workflows/{id}/status?dry_run=true"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], true, "{body}");

    // Nothing was written: still a draft, and archive-dry-run says so.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"]["status"], "draft");

    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/workflows/{id}/status?dry_run=true"),
        "archived",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body["data"]["valid"], false,
        "no active version exists yet: {body}"
    );
}

#[tokio::test]
async fn workflow_activation_dry_run_finds_the_missing_connector() {
    let app = test_app().await;
    let id = common::create_and_return_workflow_id(
        &app,
        common::workflow_with_tasks(
            "Dry Run Missing Connector",
            json!([{
                "id": "t1", "name": "Read",
                "function": {"name": "db_read",
                             "input": {"connector": "absent-db", "query": "SELECT 1"}}
            }]),
        ),
    )
    .await;

    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/workflows/{id}/status?dry_run=true"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["data"]["valid"], false, "{body}");
    let messages = body["data"]["errors"].to_string();
    assert!(messages.contains("absent-db"), "{body}");

    // The real request refuses with the same finding, as a 400.
    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/workflows/{id}/status"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("absent-db"),
        "{body}"
    );
}

#[tokio::test]
async fn dry_run_reports_a_missing_entity_as_a_finding() {
    let app = test_app().await;
    for uri in [
        "/api/v1/admin/workflows/no-such-id/status?dry_run=true",
        "/api/v1/admin/channels/no-such-id/status?dry_run=true",
    ] {
        let (code, body) = patch_status(&app, uri, "active").await;
        assert_eq!(code, StatusCode::OK, "{uri}");
        assert_eq!(body["data"]["valid"], false, "{uri}: {body}");
        assert!(
            body["data"]["errors"].to_string().contains("not found"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn channel_activation_gates_on_an_active_workflow() {
    let app = test_app().await;
    let workflow_id =
        common::create_and_return_workflow_id(&app, common::simple_log_workflow("K8 Gate")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "k8-gate",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/k8-gate",
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let status_uri = format!("/api/v1/admin/channels/{channel_id}/status");

    // Dry-run and real request agree: the workflow is a draft.
    let (code, body) = patch_status(&app, &format!("{status_uri}?dry_run=true"), "active").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["data"]["valid"], false, "{body}");
    assert!(
        body["data"]["errors"]
            .to_string()
            .contains("has no active version"),
        "{body}"
    );

    let (code, body) = patch_status(&app, &status_uri, "active").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "K8: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("activate the workflow first"),
        "{body}"
    );

    // Activate the workflow; the same channel activation now succeeds —
    // connectors → workflows → channels is the working order.
    let (code, _) = patch_status(
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}/status"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    let (code, body) = patch_status(&app, &format!("{status_uri}?dry_run=true"), "active").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["data"]["valid"], true, "{body}");

    let (code, body) = patch_status(&app, &status_uri, "active").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "active");
}

#[tokio::test]
async fn channel_activation_refuses_a_missing_or_unset_workflow() {
    let app = test_app().await;

    // Unset workflow_id: can never serve — refused outright (K8).
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "k8-no-workflow",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/k8-no-workflow",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/channels/{id}/status"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workflow_id"),
        "{body}"
    );

    // Nonexistent workflow: named, but not stored.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "k8-ghost-workflow",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/k8-ghost-workflow",
                "workflow_id": "never-created",
            })),
        ))
        .await
        .unwrap();
    let id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/channels/{id}/status"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{body}"
    );
}

#[tokio::test]
async fn channel_dry_run_sees_a_route_collision() {
    let app = test_app().await;
    common::create_and_activate_channel(
        &app,
        "collision-incumbent",
        common::simple_log_workflow("Collision Incumbent"),
    )
    .await;

    // The incumbent claimed no REST route; give the dry-run subject a real
    // collision by racing two REST channels for one pattern.
    let workflow_id =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Collision WF"))
            .await;
    common::create_rest_channel(&app, "route-owner", "/collide", vec!["POST"], &workflow_id).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "route-challenger",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/collide",
                "workflow_id": workflow_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, body) = patch_status(
        &app,
        &format!("/api/v1/admin/channels/{id}/status?dry_run=true"),
        "active",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["data"]["valid"], false, "{body}");
    assert!(
        body["data"]["errors"].to_string().contains("already"),
        "route collision must be a finding: {body}"
    );
}
