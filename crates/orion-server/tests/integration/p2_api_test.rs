//! The P2 API enablers: `GET /workflows/{id}/dependencies` (K9) and
//! `content_hash` on entity responses (K10). The transactional export
//! snapshot (K12) is covered by `export_snapshot_pages_until_exhausted` in
//! the workflows route module and the export round-trip suite.

use crate::common::{self, body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn dependencies_reports_connectors_and_channel_calls() {
    let app = test_app().await;
    let id = common::create_and_return_workflow_id(
        &app,
        json!({
            "workflow_id": "deps-wf",
            "name": "Deps",
            "tasks": [
                {"id": "t1", "name": "read",
                 "function": {"name": "db_read",
                              "input": {"connector": "orders-db", "query": "SELECT 1"}}},
                {"id": "t2", "name": "call",
                 "function": {"name": "channel_call",
                              "input": {"channel": "notify", "payload": {}}}},
                {"id": "t3", "name": "dynamic",
                 "function": {"name": "channel_call",
                              "input": {"channel": "fallback",
                                        "channel_logic": {"var": "data.target"},
                                        "payload": {}}}},
            ],
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/workflows/{id}/dependencies"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["workflow_id"], "deps-wf");
    assert_eq!(
        body["data"]["connectors"],
        json!([{"connector": "orders-db", "function": "db_read"}]),
        "{body}"
    );
    assert_eq!(
        body["data"]["channels"],
        json!(["notify", "fallback"]),
        "{body}"
    );
    assert_eq!(
        body["data"]["has_dynamic_channel_calls"], true,
        "channel_logic must flag the list as incomplete: {body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/absent/dependencies",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Equal importable content → equal hashes, across ids and DB-owned fields;
/// changed content → a different hash. This is what lets tooling compare
/// estates without fetching bodies.
#[tokio::test]
async fn content_hash_tracks_importable_content_only() {
    let app = test_app().await;
    let tasks = json!([{"id": "t1", "name": "log",
                        "function": {"name": "log", "input": {"message": "same"}}}]);
    let a = common::create_and_return_workflow_id(
        &app,
        json!({"workflow_id": "hash-a", "name": "Hash", "tasks": tasks}),
    )
    .await;
    let b = common::create_and_return_workflow_id(
        &app,
        json!({"workflow_id": "hash-b", "name": "Hash", "tasks": tasks}),
    )
    .await;

    let fetch = |id: String| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(json_request(
                    "GET",
                    &format!("/api/v1/admin/workflows/{id}"),
                    None,
                ))
                .await
                .unwrap();
            body_json(resp).await["data"]["content_hash"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let hash_a = fetch(a.clone()).await;
    let hash_b = fetch(b).await;
    assert!(hash_a.starts_with("sha256:"), "{hash_a}");
    assert_eq!(
        hash_a, hash_b,
        "identical content must hash identically across ids"
    );

    // Activation changes only DB-owned fields — the hash must not move.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{a}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        fetch(a.clone()).await,
        hash_a,
        "status/rollout are DB-owned and excluded from the hash"
    );

    // Changed content moves it.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{a}/versions"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/v1/admin/workflows/{a}"),
            Some(json!({"tasks": [{"id": "t1", "name": "log",
                                   "function": {"name": "log",
                                                "input": {"message": "different"}}}]})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_ne!(fetch(a).await, hash_a, "changed tasks must change the hash");
}
