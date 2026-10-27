use crate::common;

use crate::common::deploy;
use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Audit writes are fire-and-forget spawned tasks, so their completion is a
/// condition to poll for, not a duration to guess. Polls the admin list
/// until every `(resource_type, action)` pair is present (5s deadline).
async fn wait_for_audit_entries(app: &axum::Router, expected: &[(&str, &str)]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                "/api/v2/admin/audit-logs?limit=100",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let empty = Vec::new();
        let entries = body["data"].as_array().unwrap_or(&empty);
        if expected.iter().all(|(rt, action)| {
            entries
                .iter()
                .any(|e| e["resource_type"] == *rt && e["action"] == *action)
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit entries {expected:?} not all present within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Upload a one-task workflow definition named `name` — one audited mutation.
async fn upload_workflow(app: &axum::Router, name: &str) -> String {
    deploy::upload(
        app,
        deploy::workflow_definition(&common::simple_log_workflow(name)),
    )
    .await
}

/// Poll until the audit log holds at least `min_total` entries (5s deadline).
async fn wait_for_audit_total(app: &axum::Router, min_total: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                "/api/v2/admin/audit-logs?limit=1",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        if body["total"].as_i64().unwrap_or(0) >= min_total {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit total did not reach {min_total} within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

// ============================================================
// 1. Pagination of audit logs
// ============================================================

#[tokio::test]
async fn test_audit_pagination() {
    let app = common::test_app().await;

    // Upload 5 definitions to produce at least 5 audit entries
    for i in 0..5 {
        upload_workflow(&app, &format!("Pagination WF {}", i)).await;
    }

    wait_for_audit_total(&app, 5).await;

    // Page 1: limit=2, offset=0
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/audit-logs?limit=2&offset=0",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let page1 = body["data"].as_array().unwrap();
    assert_eq!(page1.len(), 2, "first page should have 2 entries");
    assert!(
        body["total"].as_i64().unwrap() >= 5,
        "total should be at least 5"
    );
    assert_eq!(body["offset"], 0);
    assert_eq!(body["limit"], 2);

    // Page 2: limit=2, offset=2
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/audit-logs?limit=2&offset=2",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let page2 = body["data"].as_array().unwrap();
    assert_eq!(page2.len(), 2, "second page should have 2 entries");
    assert_eq!(body["offset"], 2);
}

// ============================================================
// 2. `details` is populated end-to-end (D3)
// ============================================================

/// Before D3 the `Some(details)` branch built malformed SQL, so the column was
/// dead. Every audit write now carries the request-scoped id.
#[tokio::test]
async fn test_audit_details_carries_request_id() {
    let app = common::test_app().await;

    upload_workflow(&app, "Request Id WF").await;

    wait_for_audit_entries(&app, &[("definition", "v2_definition_upload")]).await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v2/admin/audit-logs", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let entry = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .find(|e| e["resource_type"] == "definition")
        .expect("an upload must produce an audit entry")
        .clone();

    let details = entry["details"]
        .as_str()
        .expect("details must be persisted");
    let parsed: serde_json::Value = serde_json::from_str(details).expect("details is JSON");
    assert!(
        parsed["request_id"].as_str().is_some_and(|s| !s.is_empty()),
        "details should carry the request id, got {details}"
    );
}

// ============================================================
// 3. Server-side filtering (O8)
// ============================================================

/// Seed a mix of audit entries — two definition uploads, a connection
/// create and one deployment apply naming them — and return the app plus the
/// workflow definition's digest (its upload's `resource_id`).
async fn app_with_mixed_audit_entries() -> (axum::Router, String) {
    let app = common::test_app().await;

    let workflow = common::simple_log_workflow("Filter WF");
    let wf_digest = deploy::upload(&app, deploy::workflow_definition(&workflow)).await;
    let (channel, _) =
        deploy::channel_definition(&common::sync_http_channel("filter-chan", "unused"));
    let ch_digest = deploy::upload(&app, channel).await;
    let (conn_id, conn_rev, conn_digest) =
        deploy::create_connection(&app, &common::db_connector("filter-conn")).await;
    deploy::deploy(
        &app,
        vec![
            deploy::Upsert {
                kind: "workflow",
                id: "filter-wf".to_string(),
                digest: wf_digest.clone(),
                revision: None,
                requires: Vec::new(),
            },
            deploy::Upsert {
                kind: "channel",
                id: "filter-chan".to_string(),
                digest: ch_digest,
                revision: None,
                requires: vec![("workflow", "filter-wf".to_string())],
            },
            deploy::Upsert {
                kind: "connection",
                id: conn_id,
                digest: conn_digest,
                revision: Some(conn_rev),
                requires: Vec::new(),
            },
        ],
    )
    .await;

    wait_for_audit_entries(
        &app,
        &[
            ("definition", "v2_definition_upload"),
            ("connection", "v2_connection_create"),
            ("deployment", "v2_deployment_apply"),
        ],
    )
    .await;
    wait_for_audit_total(&app, 4).await;
    (app, wf_digest)
}

async fn audit_query(app: &axum::Router, query: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/audit-logs{query}"),
            None,
        ))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn test_audit_filter_each_param_narrows_results() {
    let (app, wf_digest) = app_with_mixed_audit_entries().await;

    let (status, all) = audit_query(&app, "").await;
    assert_eq!(status, StatusCode::OK);
    let total = all["total"].as_i64().unwrap();
    assert!(total >= 4, "expected a mixed seed, got {total}");

    for (query, key, value) in [
        ("?resource_type=definition", "resource_type", "definition"),
        ("?resource_type=connection", "resource_type", "connection"),
        (
            "?action=v2_definition_upload",
            "action",
            "v2_definition_upload",
        ),
        (
            "?action=v2_deployment_apply",
            "action",
            "v2_deployment_apply",
        ),
    ] {
        let (status, body) = audit_query(&app, query).await;
        assert_eq!(status, StatusCode::OK, "query {query}");
        let entries = body["data"].as_array().unwrap();
        assert!(!entries.is_empty(), "{query} should match something");
        assert!(
            entries.iter().all(|e| e[key] == value),
            "{query} returned rows that do not match: {entries:?}"
        );
        assert!(
            body["total"].as_i64().unwrap() < total,
            "{query} must narrow the result set (total was {total})"
        );
        assert_eq!(
            body["total"].as_i64().unwrap(),
            entries.len() as i64,
            "total must count filtered rows"
        );
    }

    let (status, body) = audit_query(&app, &format!("?resource_id={wf_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["data"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| e["resource_id"] == wf_digest));

    let (status, body) = audit_query(&app, "?principal=anonymous").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"].as_i64().unwrap(),
        total,
        "auth is off in tests, so every entry is 'anonymous'"
    );

    let (status, body) = audit_query(&app, "?principal=nobody-else").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

#[tokio::test]
async fn test_audit_filters_and_together() {
    let (app, _) = app_with_mixed_audit_entries().await;

    // The documented example: ?action=…&resource_type=….
    let (status, body) =
        audit_query(&app, "?action=v2_deployment_apply&resource_type=deployment").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["resource_id"], "1", "the first revision: {body}");

    // A matching action with a non-matching type must yield nothing, not the union.
    let (status, body) =
        audit_query(&app, "?action=v2_deployment_apply&resource_type=connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_audit_time_range_filter() {
    let (app, _) = app_with_mixed_audit_entries().await;

    let (_, all) = audit_query(&app, "").await;
    let total = all["total"].as_i64().unwrap();

    let (status, body) = audit_query(&app, "?start_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), total);

    let (status, body) = audit_query(&app, "?end_time=2000-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);

    let (status, body) = audit_query(&app, "?start_time=not-a-timestamp").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
}

/// The core of O8: an unknown parameter is refused, not dropped — dropped, the
/// caller would get a 200 with the *unfiltered* table.
#[tokio::test]
async fn test_audit_unknown_query_param_is_rejected() {
    let (app, _) = app_with_mixed_audit_entries().await;

    let (status, body) = audit_query(&app, "?resource_types=workflow").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a mistyped filter must not silently return everything"
    );
    assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
    assert!(body.get("data").is_none());
}

// ============================================================
// Audit v2 (O7): actor identity, request context, drain, definition test
// ============================================================

/// Two admin keys sharing a long prefix — the case an actor recorded as the
/// first eight characters of the presented token could not tell apart.
const KEY_A: &str = "orion_sk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY_B: &str = "orion_sk_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn authed_request(
    method: &str,
    uri: &str,
    key: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {key}"))
        .header("user-agent", "orion-tests/1.0")
        .header("x-request-id", "req-audit-o7");
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(serde_json::to_string(&v).unwrap())
        }
        None => axum::body::Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn authed_app() -> axum::Router {
    let mut config = orion::config::AppConfig::default();
    config.admin_auth.enabled = true;
    config.admin_auth.api_keys = vec![KEY_A.to_string(), KEY_B.to_string()];
    common::test_app_with_config(config).await
}

/// Poll the (authenticated) audit list until `pred` holds.
async fn wait_for_audit<F>(app: &axum::Router, key: &str, pred: F) -> serde_json::Value
where
    F: Fn(&serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resp = app
            .clone()
            .oneshot(authed_request(
                "GET",
                "/api/v2/admin/audit-logs?limit=100",
                key,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        if pred(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "audit condition not met within 5s; last body: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Two keys with a shared 9-character prefix must produce two distinct
/// actors, and neither may echo any part of the credential.
#[tokio::test]
async fn audit_actor_distinguishes_keys_sharing_a_prefix() {
    let app = authed_app().await;

    for key in [KEY_A, KEY_B] {
        let resp = app
            .clone()
            .oneshot(authed_request(
                "POST",
                "/api/v2/admin/definitions",
                key,
                Some(
                    json!({"kind": "workflow", "definition": common::simple_log_workflow(
                        &format!("O7 actor {}", &key[key.len() - 4..])
                    )}),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| e["action"] == "v2_definition_upload")
                    .count()
                    >= 2
            })
            .unwrap_or(false)
    })
    .await;

    let principals: std::collections::BTreeSet<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "v2_definition_upload")
        .filter_map(|e| e["principal"].as_str())
        .collect();
    assert_eq!(
        principals.len(),
        2,
        "two distinct keys must be two distinct actors, got {principals:?}"
    );
    for p in &principals {
        assert!(p.starts_with("key-"), "unexpected actor form: {p}");
        assert!(
            !p.contains("orion_sk") && !p.contains("aaaa") && !p.contains("bbbb"),
            "the actor must not echo the credential: {p}"
        );
    }
}

/// Every audit row carries the request context an investigation needs:
/// request id, client address and user-agent.
#[tokio::test]
async fn audit_records_request_context() {
    let app = authed_app().await;

    let resp = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/v2/admin/definitions",
            KEY_A,
            Some(json!({"kind": "workflow", "definition": common::simple_log_workflow("O7 context")})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e["action"] == "v2_definition_upload")
            })
            .unwrap_or(false)
    })
    .await;
    let entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "v2_definition_upload")
        .expect("upload entry");
    let details: serde_json::Value = serde_json::from_str(
        entry["details"]
            .as_str()
            .expect("details must be present, not null"),
    )
    .expect("details must be JSON");

    assert_eq!(details["request_id"], "req-audit-o7");
    assert_eq!(details["user_agent"], "orion-tests/1.0");
    // `oneshot` supplies no ConnectInfo, so the address resolves to the same
    // "unknown" the rate limiter uses — the field is populated either way.
    assert!(
        details["client_ip"].is_string(),
        "client_ip must be recorded: {details}"
    );
}

/// O7: `POST /definitions/test` executes the workflow's tasks against
/// **live connectors**, so it is on the record, named by the digest the same
/// definition uploads to.
#[tokio::test]
async fn workflow_test_emits_an_audit_event() {
    let app = authed_app().await;
    let definition =
        json!({"kind": "workflow", "definition": common::simple_log_workflow("O7 test-run")});

    let resp = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/v2/admin/definitions/test",
            KEY_A,
            Some(json!({"definition": definition["definition"].clone(), "data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = wait_for_audit(&app, KEY_A, |b| {
        b["data"]
            .as_array()
            .map(|entries| entries.iter().any(|e| e["action"] == "v2_definition_test"))
            .unwrap_or(false)
    })
    .await;
    let entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "v2_definition_test")
        .expect("running live connectors must be on the record")
        .clone();
    assert_eq!(entry["resource_type"], "definition");

    let resp = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/v2/admin/definitions",
            KEY_A,
            Some(definition),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let digest = body_json(resp).await["digest"].clone();
    assert_eq!(entry["resource_id"], digest, "{entry}");
}

/// Wiring check for the shutdown drain: the queue an admin handler submits to
/// must be the one `TaskHandles::shutdown()` drains, and the row must be
/// readable from the database afterwards. (The drain's *timing* — that a write
/// still in flight is waited for rather than abandoned — is covered where it
/// can be forced deterministically, in
/// `queue::audit_queue::tests::shutdown_drains_events_submitted_at_the_last_moment`.)
#[tokio::test]
async fn mutations_just_before_shutdown_are_still_recorded() {
    let (state, handles) =
        common::test_state_with_handles(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let repo = state.repos.audit_logs.clone();

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/definitions",
            Some(json!({"kind": "workflow", "definition": common::simple_log_workflow("O7 shutdown")})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Exactly what main.rs does: drop the state (releasing the last audit
    // sender), then run the background-task shutdown, which drains.
    drop(state);
    handles.shutdown().await;

    let rows = repo
        .list_paginated(&Default::default())
        .await
        .expect("audit rows readable after shutdown");
    assert!(
        rows.data.iter().any(|e| e.action == "v2_definition_upload"),
        "the drain must persist a mutation accepted immediately before shutdown"
    );
}
