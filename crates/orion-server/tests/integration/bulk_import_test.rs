//! B6 (Tier-2 ergonomics): bulk import for channels/connectors + ?dry_run=true.
//!
//! Verifies:
//!   - POST /api/v1/admin/channels/import imports an array of channels and
//!     returns {imported, failed, errors[]}
//!   - POST /api/v1/admin/connectors/import same shape for connectors
//!   - ?dry_run=true on workflows/channels/connectors imports validates
//!     without writing and reports would_create / would_fail

use crate::common;
use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn channels_import_creates_each_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import",
            Some(json!([
                {
                    "name": "ch-imp-1",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/ch1"
                },
                {
                    "name": "ch-imp-2",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/ch2"
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 2);
    assert_eq!(body["data"]["failed"], 0);
}

#[tokio::test]
async fn channels_import_dry_run_does_not_persist() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import?dry_run=true",
            Some(json!([
                {
                    "name": "ch-dry-1",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/dry1"
                },
                {
                    // Will fail validation (REST without route_pattern).
                    "name": "ch-dry-broken",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 1);

    // Confirm the would-be channel was NOT actually persisted.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let data = body["data"].as_array().unwrap();
    assert!(
        data.iter().all(|ch| ch["name"] != "ch-dry-1"),
        "dry-run must not persist any rows, got {data:?}"
    );
}

#[tokio::test]
async fn connectors_import_creates_each_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import",
            Some(json!([
                { "name": "imp-a", "connector_type": "http", "config": {"url": "https://a.example.com"} },
                { "name": "imp-b", "connector_type": "http", "config": {"url": "https://b.example.com"} }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 2);
}

#[tokio::test]
async fn connectors_import_dry_run_reports_validation_outcome() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import?dry_run=true",
            Some(json!([
                { "name": "good", "connector_type": "http", "config": {"url": "https://good.example"} },
                { "name": "bad-url-scheme", "connector_type": "http", "config": {"url": "ftp://bad.example"} }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 1);
}

#[tokio::test]
async fn channels_import_dry_run_reports_enum_typo_per_item() {
    // An invalid enum used to fail the whole-batch deserialize with 400 before
    // the per-item loop ran, so dry-run couldn't preview the typo. It must now
    // be reported as a single would_fail entry.
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import?dry_run=true",
            Some(json!([
                {
                    "name": "ch-enum-ok",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/enum-ok"
                },
                {
                    "name": "ch-enum-bad",
                    "channel_type": "bogus",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/enum-bad"
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 1);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["index"], 1);
}

#[tokio::test]
async fn channels_import_real_run_skips_only_the_bad_enum_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import",
            Some(json!([
                {
                    "name": "ch-real-ok",
                    "channel_type": "sync",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/real-ok"
                },
                {
                    "name": "ch-real-bad",
                    "channel_type": "bogus",
                    "protocol": "rest",
                    "methods": ["POST"],
                    "route_pattern": "/real-bad"
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 1);
}

#[tokio::test]
async fn connectors_import_dry_run_reports_enum_typo_per_item() {
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import?dry_run=true",
            Some(json!([
                { "name": "con-enum-ok", "connector_type": "http", "config": {"url": "https://ok.example"} },
                { "name": "con-enum-bad", "connector_type": "bogus", "config": {"url": "https://bad.example"} }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 1);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["index"], 1);
}

#[tokio::test]
async fn workflows_import_dry_run_does_not_persist() {
    // Existing /workflows/import endpoint: B6 added ?dry_run=true to it.
    let app = test_app().await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import?dry_run=true",
            Some(json!([
                {
                    "name": "wf-dry",
                    "tasks": [{"id":"t1","name":"log","function":{"name":"log","input":{"message":"x"}}}]
                }
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1);
    assert_eq!(body["data"]["failed"], 0);
    // No row persisted.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/workflows", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["name"] != "wf-dry")
    );
}

// ============================================================
// R15: dry-run reads, so it can see the failure that actually happens
// ============================================================

/// R15: dry-run performed **no DB reads**, as its own doc comment said. The
/// stated use case is CI pre-flight, and the most common real failure is a name
/// conflict — precisely what a no-DB dry-run cannot see. So a green dry-run
/// said nothing about whether the real import would work.
#[tokio::test]
async fn dry_run_reports_a_conflict_with_an_already_stored_connector() {
    let app = test_app().await;
    common::create_connector(&app, common::db_connector("taken")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import?dry_run=true",
            Some(json!([
                {"name": "taken", "connector_type": "db",
                 "config": {"connection_string": "sqlite::memory:"}},
                {"name": "free", "connector_type": "db",
                 "config": {"connection_string": "sqlite::memory:"}}
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["dry_run"], true);
    assert_eq!(body["data"]["imported"], 1, "{body}");
    assert_eq!(body["data"]["failed"], 1, "{body}");
    let errors = body["data"]["errors"].as_array().expect("errors");
    assert_eq!(errors[0]["index"], 0);
    assert!(
        errors[0]["error"].as_str().unwrap_or("").contains("taken"),
        "{body}"
    );

    // And the real import agrees — which is the whole point of a pre-flight.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/connectors/import",
            Some(json!([
                {"name": "taken", "connector_type": "db",
                 "config": {"connection_string": "sqlite::memory:"}},
                {"name": "free", "connector_type": "db",
                 "config": {"connection_string": "sqlite::memory:"}}
            ])),
        ))
        .await
        .unwrap();
    let real = body_json(resp).await;
    assert_eq!(real["data"]["imported"], 1, "{real}");
    assert_eq!(real["data"]["failed"], 1, "{real}");
}

/// Duplicates *within* one batch were missed entirely, and cost nothing to
/// find: the second item conflicts with the first.
#[tokio::test]
async fn dry_run_reports_a_duplicate_inside_the_batch() {
    let app = test_app().await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import?dry_run=true",
            Some(json!([
                {"workflow_id": "same", "name": "First",
                 "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"a"}}}]},
                {"workflow_id": "same", "name": "Second",
                 "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"b"}}}]}
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 1, "{body}");
    assert_eq!(body["data"]["failed"], 1, "{body}");
    let errors = body["data"]["errors"].as_array().expect("errors");
    assert_eq!(errors[0]["index"], 1, "the *second* item is the problem");
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("more than once"),
        "{body}"
    );
}

/// An item that names no id can never conflict — the store generates one.
#[tokio::test]
async fn dry_run_does_not_invent_conflicts_for_unnamed_items() {
    let app = test_app().await;
    let item = json!({
        "name": "No Id",
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"x"}}}]
    });
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import?dry_run=true",
            Some(json!([item.clone(), item])),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 2, "{body}");
    assert_eq!(body["data"]["failed"], 0, "{body}");
}

// ============================================================
// R19: one import driver, one failure semantic
// ============================================================

/// D24: partial success must be *real*. The old `bulk_create` collected
/// per-row results inside one shared transaction and unconditionally
/// committed — on SQLite/Postgres a failed row aborts the transaction, so the
/// `imported`/`failed` counts described writes that never happened. The
/// per-item driver (R19) commits each row independently; this pins that a
/// `[valid, invalid, valid]` batch really persists exactly the two rows it
/// reports as imported.
#[tokio::test]
async fn workflows_import_partial_success_actually_persists_the_valid_rows() {
    let app = test_app().await;
    let task = json!([{"id": "t1", "name": "Log",
                       "function": {"name": "log", "input": {"message": "x"}}}]);
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/import",
            Some(json!([
                {"workflow_id": "imp-valid-1", "name": "Valid One", "tasks": task.clone()},
                // Unknown function name — rejected by create-path validation.
                {"workflow_id": "imp-invalid", "name": "Broken",
                 "tasks": [{"id": "t1", "name": "Nope",
                            "function": {"name": "no_such_function", "input": {}}}]},
                {"workflow_id": "imp-valid-2", "name": "Valid Two", "tasks": task},
            ])),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 2, "{body}");
    assert_eq!(body["data"]["failed"], 1, "{body}");
    let errors = body["data"]["errors"].as_array().expect("errors");
    assert_eq!(errors[0]["index"], 1, "{body}");

    // The counts must not be fiction: both valid rows are queryable…
    for id in ["imp-valid-1", "imp-valid-2"] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/admin/workflows/{id}"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "'{id}' was reported imported but is not stored"
        );
    }
    // …and the invalid one is not.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/imp-invalid",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// R19: workflows drove `bulk_create` over a typed `Vec<CreateWorkflowRequest>`,
/// so **one malformed item aborted the whole batch with a 400** — while the
/// identical mistake against channels or connectors produced one failed entry
/// and imported the rest. Three endpoints, one documented shape, two behaviours.
#[tokio::test]
async fn a_malformed_item_fails_alone_on_every_import_endpoint() {
    let good_workflow = json!({
        "name": "Good",
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"x"}}}]
    });
    let good_channel = json!({
        "name": "good-ch", "channel_type": "sync", "protocol": "http",
        "methods": ["POST"], "route_pattern": "/good-ch"
    });
    let good_connector = json!({
        "name": "good-conn", "connector_type": "db",
        "config": {"connection_string": "sqlite::memory:"}
    });
    // Missing `name` in each case — a shape error, not a validation one.
    let malformed = json!({"nope": true});

    for (path, good) in [
        ("/api/v1/admin/workflows/import", good_workflow),
        ("/api/v1/admin/channels/import", good_channel),
        ("/api/v1/admin/connectors/import", good_connector),
    ] {
        let app = test_app().await;
        let resp = app
            .oneshot(json_request(
                "POST",
                path,
                Some(json!([malformed.clone(), good])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path} must not 400");
        let body = body_json(resp).await;
        assert_eq!(body["data"]["imported"], 1, "{path}: {body}");
        assert_eq!(body["data"]["failed"], 1, "{path}: {body}");
        assert_eq!(
            body["data"]["errors"][0]["index"], 0,
            "{path}: the failure must be attributed to the item that caused it"
        );
    }
}

// ============================================================
// K2: ?on_conflict= — skip and new_version (upsert) modes
// ============================================================

/// The full upsert ladder for a versioned entity: created → unchanged
/// (idempotent re-run) → updated_draft → new_version over an active row.
#[tokio::test]
async fn workflow_upsert_ladder() {
    let app = test_app().await;
    let uri = "/api/v1/admin/workflows/import?on_conflict=new_version";
    let item = |message: &str| {
        json!([{
            "workflow_id": "wf-upsert",
            "name": "Upsert",
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": message}}}]
        }])
    };

    // 1. Absent id → created.
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("v1"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "created", "{body}");
    assert_eq!(body["data"]["imported"], 1);

    // 2. Same content again → unchanged, nothing imported. The idempotency
    //    a CI re-run rides on.
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("v1"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "unchanged", "{body}");
    assert_eq!(body["data"]["imported"], 0);
    assert_eq!(body["data"]["unchanged"], 1);

    // 3. Changed content over the existing draft → updated_draft, in place.
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("v2"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["results"][0]["action"], "updated_draft",
        "{body}"
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/wf-upsert",
            None,
        ))
        .await
        .unwrap();
    let wf = body_json(resp).await;
    assert_eq!(wf["data"]["version"], 1, "draft updated in place: {wf}");
    assert_eq!(wf["data"]["tasks"][0]["function"]["input"]["message"], "v2");

    // 4. Activate, then import changed content → a new draft version.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/v1/admin/workflows/wf-upsert/status",
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Active + identical → unchanged (the active row is the comparison).
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("v2"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "unchanged", "{body}");

    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("v3"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["results"][0]["action"], "new_version",
        "{body}"
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/wf-upsert",
            None,
        ))
        .await
        .unwrap();
    let wf = body_json(resp).await;
    assert_eq!(wf["data"]["version"], 2, "{wf}");
    assert_eq!(wf["data"]["status"], "draft");
    assert_eq!(wf["data"]["tasks"][0]["function"]["input"]["message"], "v3");
}

/// Dry-run reports the action the real import would take, and writes nothing.
#[tokio::test]
async fn upsert_dry_run_reports_the_would_be_action() {
    let app = test_app().await;
    let real = "/api/v1/admin/workflows/import?on_conflict=new_version";
    let dry = "/api/v1/admin/workflows/import?on_conflict=new_version&dry_run=true";
    let item = |message: &str| {
        json!([{
            "workflow_id": "wf-upsert-dry",
            "name": "Upsert Dry",
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": message}}}]
        }])
    };

    for (uri, expected) in [(dry, "created"), (real, "created"), (dry, "unchanged")] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", uri, Some(item("v1"))))
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert_eq!(
            body["data"]["results"][0]["action"], expected,
            "{uri}: {body}"
        );
    }

    // The dry run of changed content predicts updated_draft — and left the
    // stored draft untouched.
    let resp = app
        .clone()
        .oneshot(json_request("POST", dry, Some(item("v2"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["results"][0]["action"], "updated_draft",
        "{body}"
    );
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/workflows/wf-upsert-dry",
            None,
        ))
        .await
        .unwrap();
    let wf = body_json(resp).await;
    assert_eq!(wf["data"]["tasks"][0]["function"]["input"]["message"], "v1");
}

/// `skip` leaves the stored entity alone; the in-batch duplicate is refused
/// in the upsert-ish modes rather than silently self-overwriting.
#[tokio::test]
async fn skip_mode_and_in_batch_duplicates() {
    let app = test_app().await;
    let create = json!([{
        "channel_id": "ch-skip",
        "name": "ch-skip",
        "channel_type": "sync",
        "protocol": "http",
        "methods": ["POST"],
        "route_pattern": "/ch-skip",
    }]);
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import",
            Some(create),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"]["imported"], 1);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import?on_conflict=skip",
            Some(json!([
                {"channel_id": "ch-skip", "name": "renamed",
                 "channel_type": "sync", "protocol": "http",
                 "methods": ["POST"], "route_pattern": "/ch-skip"},
                {"channel_id": "ch-skip-new", "name": "ch-skip-new",
                 "channel_type": "sync", "protocol": "http",
                 "methods": ["POST"], "route_pattern": "/ch-skip-new"},
            ])),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["skipped"], 1, "{body}");
    assert_eq!(body["data"]["imported"], 1, "{body}");
    assert_eq!(body["data"]["results"][0]["action"], "skipped");
    assert_eq!(body["data"]["results"][1]["action"], "created");

    // The skipped channel kept its stored name.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/channels/ch-skip", None))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"]["name"], "ch-skip");

    // In-batch duplicate under new_version: the second item fails.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/import?on_conflict=new_version",
            Some(json!([
                {"channel_id": "ch-dup", "name": "ch-dup",
                 "channel_type": "sync", "protocol": "http",
                 "methods": ["POST"], "route_pattern": "/ch-dup"},
                {"channel_id": "ch-dup", "name": "ch-dup-again",
                 "channel_type": "sync", "protocol": "http",
                 "methods": ["POST"], "route_pattern": "/ch-dup"},
            ])),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["imported"], 1, "{body}");
    assert_eq!(body["data"]["failed"], 1, "{body}");
    assert!(
        body["data"]["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("more than once in this batch"),
        "{body}"
    );
}

/// Connectors are unversioned: upsert updates in place, and the round trip
/// carries `enabled` (K1) so a disabled connector no longer promotes enabled.
#[tokio::test]
async fn connector_upsert_updates_in_place_and_keeps_enabled() {
    let app = test_app().await;
    let uri = "/api/v1/admin/connectors/import?on_conflict=new_version";
    let item = |url: &str| {
        json!([{
            "name": "upsert-http",
            "connector_type": "http",
            "config": {"url": url},
            "enabled": false,
        }])
    };

    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("https://one.example"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "created", "{body}");

    // K1: the disabled flag survived the import.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors/export", None))
        .await
        .unwrap();
    let export = body_json(resp).await;
    let row = export["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "upsert-http")
        .expect("exported row");
    assert_eq!(row["enabled"], false, "K1: {row}");

    // Identical content → unchanged.
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("https://one.example"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "unchanged", "{body}");

    // Changed config → updated in place (connectors have no versions).
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(item("https://two.example"))))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["results"][0]["action"], "updated", "{body}");

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/connectors/export", None))
        .await
        .unwrap();
    let export = body_json(resp).await;
    let row = export["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "upsert-http")
        .expect("exported row");
    assert_eq!(row["config"]["url"], "https://two.example", "{row}");
}
