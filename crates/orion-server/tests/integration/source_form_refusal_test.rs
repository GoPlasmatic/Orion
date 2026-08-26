//! The admin API names an uncompiled reference instead of its symptom (#295).
//!
//! `$from` and `use` are resolved when a definition set is compiled, and the
//! admin API compiles nothing — it takes one document with no set to resolve
//! names against. That much is deliberate. What was not is how the refusal
//! read: the reference reached the function-input validator as literal JSON
//! and was refused for the fields it would have supplied, so an author went
//! looking for a typo that was not there.
//!
//! Each test below is one of the shapes that was confirmed broken before the
//! fix, with the message it produced then in the comment.

use crate::common::{self, body_json, json_request};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// `(status, code, path, message)` of the first field error.
async fn post(uri: &str, body: Value) -> (StatusCode, Value) {
    let app = common::test_app().await;
    let resp = app
        .oneshot(json_request("POST", uri, Some(body)))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

fn detail<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["error"]["details"]
        .as_array()
        .unwrap_or_else(|| panic!("expected details in {body}"))
        .iter()
        .find(|d| d["path"] == path)
        .unwrap_or_else(|| panic!("no detail at '{path}' in {body}"))
}

/// Before: `tasks[1].function.input.connector REQUIRED` and
/// `…input.database REQUIRED` — the two fields `constants.db` would have
/// supplied, and nothing naming `$from`.
#[tokio::test]
async fn a_from_in_a_task_input_is_named_not_its_missing_fields() {
    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({
            "workflow_id": "probe", "name": "Probe",
            "tasks": [
                {"id": "t1", "name": "T1", "function": {"name": "map",
                  "input": {"mappings": [{"path": "data.out", "logic": {"ok": true}}]}}},
                {"id": "t2", "name": "T2", "function": {"name": "mongo_read",
                  "input": {"$from": "constants.db", "collection": "users",
                            "filter": {}, "output": "temp_data.u"}}}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "tasks[1].function.input");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(
        message.contains(r#"{"$from": "constants.db"}"#),
        "the message must show the reference as authored: {message}"
    );
    assert!(
        message.contains("orion-server compile"),
        "and name the command that resolves it: {message}"
    );
    // The symptom must not be reported alongside the cause: two errors about
    // fields that are not missing is what sent the author looking for a typo.
    let paths: Vec<&str> = body["error"]["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["tasks[1].function.input"], "{body}");
}

/// Before: `tasks[0].name REQUIRED` and `tasks[0].function.name REQUIRED` — a
/// fragment call site looks to the validator like a task missing both.
#[tokio::test]
async fn a_use_step_is_named_not_its_missing_task_fields() {
    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({
            "workflow_id": "probe", "name": "Probe",
            "tasks": [
                {"id": "_session", "use": "require-session", "with": {"deny_message": "x"}},
                {"id": "t1", "name": "T1", "function": {"name": "map",
                  "input": {"mappings": [{"path": "data.out", "logic": {"ok": true}}]}}}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "tasks[0]");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(
        message.contains(r#"{"use": "require-session"}"#),
        "{message}"
    );
    assert!(message.contains("task-fragment reference"), "{message}");
}

/// The worst of the three, and the one that was not an error message problem
/// at all: this was accepted with **201** and stored, and the workflow then
/// wrote the literal `{"$from": …}` object into `data.out` at runtime.
#[tokio::test]
async fn a_from_deep_in_a_payload_is_refused_rather_than_stored() {
    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({
            "workflow_id": "probe", "name": "Probe",
            "tasks": [
                {"id": "t1", "name": "T1", "function": {"name": "map",
                  "input": {"mappings": [
                    {"path": "data.out", "logic": {"$from": "errors.USER_NOT_FOUND"}}]}}}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let d = detail(&body, "tasks[0].function.input.mappings[0].logic");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
}

/// A workflow's other JSON-bearing fields are spliced too, so a check that
/// covered only `tasks` would let these through to be stored uncompiled.
#[tokio::test]
async fn condition_and_loop_are_checked_as_well_as_tasks() {
    let task = json!({"id": "t", "name": "T", "function": {"name": "map",
        "input": {"mappings": [{"path": "data.ok", "logic": true}]}}});

    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({"workflow_id": "p1", "name": "P", "tasks": [task.clone()],
               "condition": {"$from": "constants.guard"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(detail(&body, "condition")["code"], "UNCOMPILED_SOURCE");

    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({"workflow_id": "p2", "name": "P", "tasks": [task],
               "loop": {"$from": "constants.sweep"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(detail(&body, "loop")["code"], "UNCOMPILED_SOURCE");
}

/// Before: `Invalid connector config for type 'db': missing field
/// connection_string` — the field the shared value carries.
#[tokio::test]
async fn a_connector_config_names_the_reference() {
    let (status, body) = post(
        "/api/v1/admin/connectors",
        json!({"name": "probe", "connector_type": "db",
               "config": {"$from": "constants.db"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "connector.config");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    assert!(
        d["message"].as_str().unwrap().contains("constants.db"),
        "{body}"
    );
}

/// Before: `unknown field '$from', expected one of rate_limit, timeout_ms, …`
/// — which names the key but reads as a typo, and offers a list of spellings
/// none of which is what the author meant.
#[tokio::test]
async fn a_channel_config_says_what_the_key_is_rather_than_offering_spellings() {
    let (status, body) = post(
        "/api/v1/admin/channels",
        json!({"name": "probe-ch", "channel_type": "sync", "protocol": "rest",
               "methods": ["POST"], "route_pattern": "/probe",
               "config": {"$from": "constants.guards", "timeout_ms": 1000}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "channel.config");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(!message.contains("unknown field"), "{message}");
    assert!(message.contains("shared-value reference"), "{message}");
}

/// `/validate` reuses the create-path validator, so it must agree with create
/// — a linter that green-lit a payload create rejects is worse than no linter.
#[tokio::test]
async fn validate_agrees_with_create() {
    let app = common::test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "workflow_id": "probe", "name": "Probe",
                "tasks": [{"id": "t", "name": "T", "function": {"name": "mongo_read",
                    "input": {"$from": "constants.db", "collection": "u",
                              "filter": {}, "output": "temp_data.u"}}}]
            })),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false, "{body}");
    let rendered = body.to_string();
    assert!(rendered.contains("constants.db"), "{body}");
}

/// The update path stores into the same column, so it needs the same gate.
#[tokio::test]
async fn the_update_path_is_gated_too() {
    let app = common::test_app().await;
    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "workflow_id": "upd", "name": "Upd",
                "tasks": [{"id": "t", "name": "T", "function": {"name": "map",
                    "input": {"mappings": [{"path": "data.ok", "logic": true}]}}}]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let resp = app
        .oneshot(json_request(
            "PUT",
            "/api/v1/admin/workflows/upd",
            Some(json!({"tasks": [{"id": "t", "name": "T",
                "function": {"name": "mongo_read", "input": {"$from": "constants.db",
                    "collection": "u", "filter": {}, "output": "temp_data.u"}}}]})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        detail(&body, "tasks[0].function.input")["code"],
        "UNCOMPILED_SOURCE"
    );
}

/// The refusal must be precise about what a reference *is*, or it becomes a
/// new way for a valid workflow to be rejected. A payload field named `use`,
/// a `tasks` array that is a function input rather than a step list, and a
/// non-string `$from` are all things the compiler leaves alone — so the API
/// must accept them.
#[tokio::test]
async fn a_payload_that_merely_resembles_a_reference_still_creates() {
    let (status, body) = post(
        "/api/v1/admin/workflows",
        json!({
            "workflow_id": "lookalike", "name": "Lookalike",
            "tasks": [{"id": "t", "name": "T", "function": {"name": "map",
                "input": {"mappings": [
                    {"path": "data.a", "logic": {"use": "cache"}},
                    {"path": "data.b", "logic": {"tasks": [{"use": "nested"}]}},
                    {"path": "data.c", "logic": {"$from": 5}}]}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}
