//! A definition upload names an uncompiled reference instead of its symptom
//! (#295).
//!
//! `$from` and `use` are resolved when a definition set is compiled, and the
//! admin API compiles nothing — `POST /definitions` takes one document with no
//! set to resolve names against. The refusal must identify the compilation
//! step instead of treating a reference as ordinary function-input JSON.
//!
//! Each shape must name its uncompiled reference instead of reporting the
//! fields that reference would supply.

use crate::common;
use axum::http::StatusCode;
use serde_json::{Value, json};

/// Upload a v1-shaped workflow body as a workflow definition, answering the
/// status and body.
async fn upload(body: Value) -> (StatusCode, Value) {
    let app = common::test_app().await;
    common::deploy::try_upload(&app, common::deploy::workflow_definition(&body)).await
}

fn detail<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["error"]["details"]
        .as_array()
        .unwrap_or_else(|| panic!("expected details in {body}"))
        .iter()
        .find(|d| d["path"] == path)
        .unwrap_or_else(|| panic!("no detail at '{path}' in {body}"))
}

/// Name `$from` instead of reporting a missing connector that `constants.db` supplies.
#[tokio::test]
async fn a_from_in_a_task_input_is_named_not_its_missing_fields() {
    let (status, body) = upload(json!({
        "workflow_id": "probe", "name": "Probe",
        "tasks": [
            {"id": "t1", "name": "T1", "function": {"name": "map",
              "input": {"mappings": [{"path": "data.out", "logic": {"ok": true}}]}}},
            {"id": "t2", "name": "T2", "function": {"name": "data_query",
              "input": {"$from": "constants.db", "query": {"source": "users"},
                        "output": "temp_data.u"}}}]
    }))
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "definition.tasks[1].function.input");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(
        message.contains(r#"{"$from": "constants.db"}"#),
        "the message must show the reference as authored: {message}"
    );
    assert!(
        message.contains("orion-cli compile"),
        "and name the command that resolves it: {message}"
    );
    // The symptom must not be reported alongside the cause: errors about
    // fields that are not missing is what sent the author looking for a typo.
    let paths: Vec<&str> = body["error"]["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["definition.tasks[1].function.input"], "{body}");
}

/// Name the fragment call rather than treating it as a task missing its fields.
#[tokio::test]
async fn a_use_step_is_named_not_its_missing_task_fields() {
    let (status, body) = upload(json!({
        "workflow_id": "probe", "name": "Probe",
        "tasks": [
            {"id": "_session", "use": "require-session", "with": {"deny_message": "x"}},
            {"id": "t1", "name": "T1", "function": {"name": "map",
              "input": {"mappings": [{"path": "data.out", "logic": {"ok": true}}]}}}]
    }))
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let d = detail(&body, "definition.tasks[0]");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(
        message.contains(r#"{"use": "require-session"}"#),
        "{message}"
    );
    assert!(message.contains("task-fragment reference"), "{message}");
}

/// Refuse a nested uncompiled reference instead of storing its literal object
/// as workflow data.
#[tokio::test]
async fn a_from_deep_in_a_payload_is_refused_rather_than_stored() {
    let (status, body) = upload(json!({
        "workflow_id": "probe", "name": "Probe",
        "tasks": [
            {"id": "t1", "name": "T1", "function": {"name": "map",
              "input": {"mappings": [
                {"path": "data.out", "logic": {"$from": "errors.USER_NOT_FOUND"}}]}}}]
    }))
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let d = detail(
        &body,
        "definition.tasks[0].function.input.mappings[0].logic",
    );
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
}

/// A workflow's other JSON-bearing fields are spliced too, so a check that
/// covered only `tasks` would let these through to be stored uncompiled.
#[tokio::test]
async fn condition_and_loop_are_checked_as_well_as_tasks() {
    let task = json!({"id": "t", "name": "T", "function": {"name": "map",
        "input": {"mappings": [{"path": "data.ok", "logic": true}]}}});

    let (status, body) = upload(
        json!({"workflow_id": "p1", "name": "P", "tasks": [task.clone()],
               "condition": {"$from": "constants.guard"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        detail(&body, "definition.condition")["code"],
        "UNCOMPILED_SOURCE"
    );

    let (status, body) = upload(json!({"workflow_id": "p2", "name": "P", "tasks": [task],
               "loop": {"$from": "constants.sweep"}}))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        detail(&body, "definition.loop")["code"],
        "UNCOMPILED_SOURCE"
    );
}

/// A channel config reference must name the compilation step that resolves it.
/// Both channel uploads and channel definition uploads use this validation.
#[tokio::test]
async fn a_channel_config_says_what_the_key_is_rather_than_offering_spellings() {
    let app = common::test_app().await;
    let (status, body) = common::deploy::try_upload(
        &app,
        json!({"kind": "channel", "definition": {
            "name": "probe-ch", "channel_type": "sync", "protocol": "rest",
            "methods": ["POST"], "route_pattern": "/probe",
            "config": {"$from": "constants.guards", "timeout_ms": 1000}}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let d = detail(&body, "definition.config");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(!message.contains("unknown field"), "{message}");
    assert!(message.contains("shared-value reference"), "{message}");
}

/// The refusal must be precise about what a reference *is*, or it becomes a
/// new way for a valid workflow to be rejected. A payload field named `use`,
/// a `tasks` array that is a function input rather than a step list, and a
/// non-string `$from` are all things the compiler leaves alone — so the API
/// must accept them.
#[tokio::test]
async fn a_payload_that_merely_resembles_a_reference_still_creates() {
    let (status, body) = upload(json!({
        "workflow_id": "lookalike", "name": "Lookalike",
        "tasks": [{"id": "t", "name": "T", "function": {"name": "map",
            "input": {"mappings": [
                {"path": "data.a", "logic": {"use": "cache"}},
                {"path": "data.b", "logic": {"tasks": [{"use": "nested"}]}},
                {"path": "data.c", "logic": {"$from": 5}},
                {"path": "data.d", "logic": {"$sql": 5}},
                {"path": "data.e", "logic": {"$use": 5}},
                {"path": "data.f", "logic": {"$each": "x"}}]}}}]
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// #332: a `$sql` file reference is compiled by `compile`, not the API — so
/// the API names it rather than refusing the statement's absence.
#[tokio::test]
async fn a_sql_reference_is_named_by_uncompiled_source() {
    let (status, body) = upload(json!({
        "workflow_id": "sqlref", "name": "SQL ref",
        "tasks": [{"id": "r", "name": "R", "function": {"name": "db_read",
            "input": {"connector": "db", "query": {"$sql": "sql/read.sql"}}}}]
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let d = detail(&body, "definition.tasks[0].function.input.query");
    assert_eq!(d["code"], "UNCOMPILED_SOURCE");
    let message = d["message"].as_str().unwrap();
    assert!(message.contains("sql/read.sql"), "{message}");
}

/// #333: a `$use` and an `$each` are compiled by `compile`, not the API —
/// each is named, where it sits, rather than stored as a literal key.
#[tokio::test]
async fn a_value_fragment_and_a_repetition_are_named() {
    let (status, body) = upload(json!({
        "workflow_id": "sugar", "name": "Sugar",
        "condition": {"$use": "positive", "with": {"field": "total"}},
        "tasks": [{"$each": {"p": [0, 1]}, "do": {"id": "t{{p}}", "name": "T",
            "function": {"name": "map", "input": {"mappings": []}}}}]
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let condition = detail(&body, "definition.condition");
    assert_eq!(condition["code"], "UNCOMPILED_SOURCE");
    assert!(
        condition["message"].as_str().unwrap().contains("positive"),
        "{condition}"
    );
    let each = detail(&body, "definition.tasks[0]");
    assert_eq!(each["code"], "UNCOMPILED_SOURCE", "{body}");
    assert!(
        each["message"].as_str().unwrap().contains("$each"),
        "{each}"
    );
}
