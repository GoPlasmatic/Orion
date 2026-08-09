//! The P1 promotion items: deferred reload (K4), change-context auditing and
//! per-entity import audit rows (K5), tags on channels and connectors (K6),
//! and unique channel names (K7).

use crate::common::{self, body_json, json_request, test_app};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request("GET", uri, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    body_json(resp).await
}

// ---------------------------------------------------------------------------
// K4 — ?reload=defer
// ---------------------------------------------------------------------------

/// A deferred activation commits the row but leaves the engine on the old
/// active set; the explicit reload is what makes it serve.
#[tokio::test]
async fn deferred_reload_batches_into_one_explicit_reload() {
    let app = test_app().await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(common::simple_log_workflow("Deferred Reload")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["data"]["workflow_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{id}/status?reload=defer"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The channel that serves the workflow, activated with defer too — the
    // apply order a bundle uses (the K8 gate reads the *stored* active set,
    // so the deferred workflow activation above satisfies it).
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "deferred-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/deferred-ch",
                "workflow_id": id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let ch_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{ch_id}/status?reload=defer"),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Both rows are active…
    let body = get(&app, &format!("/api/v1/admin/workflows/{id}")).await;
    assert_eq!(body["data"]["status"], "active");
    // …but the engine was not rebuilt: nothing is loaded, nothing serves.
    let status = get(&app, "/api/v1/admin/engine/status").await;
    assert_eq!(
        status["data"]["workflows_count"], 0,
        "defer must leave the engine on the previous active set: {status}"
    );

    // One explicit reload picks the whole batch up.
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = get(&app, "/api/v1/admin/engine/status").await;
    assert_eq!(status["data"]["workflows_count"], 1, "{status}");
    assert_eq!(
        status["data"]["channels"],
        json!(["deferred-ch"]),
        "{status}"
    );
}

// ---------------------------------------------------------------------------
// K5 — change context + per-entity import audit
// ---------------------------------------------------------------------------

/// `X-Orion-Change-Context` lands in the audit row's `details`, and an import
/// writes one row per created entity alongside the batch summary.
#[tokio::test]
async fn change_context_and_per_entity_rows_reach_the_audit_trail() {
    let app = test_app().await;

    let items = json!([
        {"workflow_id": "k5-wf-a", "name": "K5 A",
         "tasks": [{"id": "t1", "name": "log",
                    "function": {"name": "log", "input": {"message": "a"}}}]},
        {"workflow_id": "k5-wf-b", "name": "K5 B",
         "tasks": [{"id": "t1", "name": "log",
                    "function": {"name": "log", "input": {"message": "b"}}}]},
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/admin/workflows/import")
        .header("content-type", "application/json")
        .header("x-orion-change-context", "package=payments@1.4.0")
        .body(Body::from(serde_json::to_vec(&items).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["imported"], 2);

    // The audit queue is asynchronous — poll until the rows land.
    let body = common::wait_for_body(
        &app,
        "/api/v1/admin/audit-logs?resource_type=workflow&action=import",
        |body| body["data"].as_array().is_some_and(|rows| rows.len() >= 3),
    )
    .await;
    let rows = body["data"].as_array().unwrap();
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["resource_id"].as_str())
        .collect();
    assert!(
        ids.contains(&"k5-wf-a") && ids.contains(&"k5-wf-b"),
        "{ids:?}"
    );
    assert!(ids.contains(&"2 imported"), "summary row kept: {ids:?}");
    for row in rows {
        let details: Value =
            serde_json::from_str(row["details"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        assert_eq!(
            details["change_context"], "package=payments@1.4.0",
            "every row of the operation carries the context: {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// K6 — tags on channels and connectors
// ---------------------------------------------------------------------------

/// Tags round-trip through create → read → export, and `?tag=` selects on
/// list and export for both kinds.
#[tokio::test]
async fn tags_select_channels_and_connectors() {
    let app = test_app().await;

    for (name, tags) in [("tagged-ch", json!(["pkg:pay"])), ("plain-ch", json!([]))] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/channels",
                Some(json!({
                    "name": name,
                    "channel_type": "sync",
                    "protocol": "http",
                    "methods": ["POST"],
                    "route_pattern": format!("/{name}"),
                    "tags": tags,
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "{name}");
    }
    for (name, tags) in [
        ("tagged-conn", json!(["pkg:pay"])),
        ("plain-conn", json!([])),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/connectors",
                Some(json!({
                    "name": name,
                    "connector_type": "http",
                    "config": {"url": "https://example.com"},
                    "tags": tags,
                })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "{name}");
    }

    // List filters.
    let body = get(&app, "/api/v1/admin/channels?tag=pkg%3Apay").await;
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(names, ["tagged-ch"], "{body}");
    assert_eq!(body["data"][0]["tags"], json!(["pkg:pay"]));

    let body = get(&app, "/api/v1/admin/connectors?tag=pkg%3Apay").await;
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(names, ["tagged-conn"], "{body}");

    // Export filters, and the artifact carries the tags for the next import.
    let body = get(&app, "/api/v1/admin/channels/export?tag=pkg%3Apay").await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["data"][0]["tags"], json!(["pkg:pay"]));

    let body = get(&app, "/api/v1/admin/connectors/export?tag=pkg%3Apay").await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["data"][0]["tags"], json!(["pkg:pay"]));
}

/// Tag content participates in the K2 idempotency comparison: same tags →
/// unchanged; changed tags → a write.
#[tokio::test]
async fn tags_participate_in_upsert_convergence() {
    let app = test_app().await;
    let uri = "/api/v1/admin/channels/import?on_conflict=new_version";
    let item = |tags: Value| {
        json!([{
            "channel_id": "tag-upsert",
            "name": "tag-upsert",
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": "/tag-upsert",
            "tags": tags,
        }])
    };

    for (tags, expected) in [
        (json!(["v1"]), "created"),
        (json!(["v1"]), "unchanged"),
        (json!(["v2"]), "updated_draft"),
    ] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", uri, Some(item(tags.clone()))))
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert_eq!(
            body["data"]["results"][0]["action"], expected,
            "tags {tags}: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// K7 — unique channel names
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_channel_names_are_refused() {
    let app = test_app().await;
    let channel = |id: &str, name: &str| {
        json!({
            "channel_id": id,
            "name": name,
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": format!("/{id}"),
        })
    };

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(channel("holder", "shared-name")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A second id claiming the name → 409, naming the holder.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(channel("challenger", "shared-name")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("holder"),
        "{body}"
    );

    // Renaming a draft onto the occupied name → 409 too.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(channel("renamer", "other-name")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/channels/renamer",
            Some(json!({"name": "shared-name"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Same-id operations stay legal: a new version repeats its own name.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/channels/holder",
            Some(json!({"description": "still mine"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Import path goes through the same gate: one per-item 409-shaped error.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import",
            Some(json!([channel("import-challenger", "shared-name")])),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["failed"], 1, "{body}");
    assert!(
        body["data"]["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("shared-name"),
        "{body}"
    );
}
