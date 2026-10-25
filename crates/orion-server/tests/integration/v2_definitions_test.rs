//! Definitions over HTTP: a service deployed through v2 alone — definitions
//! uploaded, a manifest naming them applied, the channel serving — and what
//! an upload or a manifest is turned away for.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{json_request, test_app};

const DEFINITIONS: &str = "/api/v2/admin/definitions";
const PLANS: &str = "/api/v2/admin/deployment-plans";
const DEPLOYMENTS: &str = "/api/v2/admin/deployments";

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, uri, body))
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    // A body the JSON extractor refuses answers in plain text.
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}

fn greeting_workflow() -> Value {
    json!({"kind": "workflow", "definition": {
        "name": "Greet",
        "tasks": [{"id": "greet", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.greeting", "logic": {"cat": ["Hello, ", {"var": "data.name"}]}}
        ]}}}]
    }})
}

fn greeting_channel() -> Value {
    json!({"kind": "channel", "definition": {
        "name": "greet",
        "protocol": "rest",
        "methods": ["POST"],
        "route_pattern": "/greet",
    }})
}

async fn upload(app: &Router, definition: Value) -> String {
    let (status, body) = send(app, "POST", DEFINITIONS, Some(definition)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["digest"].as_str().expect("a digest").to_string()
}

fn manifest(channel: &str, workflow: &str) -> Value {
    json!({
        "schema_version": 2,
        "providers": [],
        "members": [
            {"entity": {"kind": "channel", "id": "greet", "revision": 1},
             "definition_digest": channel,
             "requires": [{"kind": "workflow", "id": "greet-wf", "revision": 1}]},
            {"entity": {"kind": "workflow", "id": "greet-wf", "revision": 1},
             "definition_digest": workflow},
        ]
    })
}

/// A service deployed through v2 alone serves: nothing here touches the
/// draft/active entity routes.
#[tokio::test]
async fn a_service_deployed_from_definitions_serves() {
    let app = test_app().await;
    let workflow = upload(&app, greeting_workflow()).await;
    let channel = upload(&app, greeting_channel()).await;

    let (status, applied) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        Some(json!({
            "idempotency_key": "greet-1",
            "base_revision": 0,
            "manifest": manifest(&channel, &workflow),
            "reason": "deploy greet",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{applied}");
    assert_eq!(applied["evaluation"]["converged"], true, "{applied}");

    let (status, answer) = send(
        &app,
        "POST",
        "/api/v2/data/greet",
        Some(json!({"data": {"name": "Ada"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["data"]["greeting"], "Hello, Ada");
}

/// One definition, however it is spelled, is one digest; and it reads back
/// as stored.
#[tokio::test]
async fn an_upload_is_idempotent_and_reads_back() {
    let app = test_app().await;
    let first = upload(&app, greeting_workflow()).await;
    let mut spelled = greeting_workflow();
    spelled["definition"]["on_failure"] = json!("fail");
    spelled["definition"]["priority"] = json!(0);
    assert_eq!(upload(&app, spelled).await, first);

    let (status, stored) = send(&app, "GET", &format!("{DEFINITIONS}/{first}"), None).await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["kind"], "workflow");
    assert_eq!(stored["definition"]["name"], "Greet");

    let missing = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let (status, _) = send(&app, "GET", &format!("{DEFINITIONS}/{missing}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A definition that would not serve is refused at upload, with the problem
/// at its field under `definition`.
#[tokio::test]
async fn a_definition_that_would_not_serve_is_refused() {
    let app = test_app().await;
    let cases = [
        // A near-miss key the loader would quarantine (D9).
        (
            json!({"kind": "workflow", "definition": {"name": "w", "tasks": [
                {"id": "t", "conditon": false, "function": {"name": "log", "input": {"message": "x"}}}
            ]}}),
            "definition.tasks[0].conditon",
        ),
        // A function no node has.
        (
            json!({"kind": "workflow", "definition": {"name": "w", "tasks": [
                {"id": "t", "function": {"name": "no_such_function", "input": {}}}
            ]}}),
            "definition.tasks[0]",
        ),
        // A route pattern that is not one.
        (
            json!({"kind": "channel", "definition": {
                "name": "c", "protocol": "rest", "route_pattern": "no-leading-slash"
            }}),
            "definition",
        ),
    ];
    for (definition, at) in cases {
        let (status, body) = send(&app, "POST", DEFINITIONS, Some(definition.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{definition}: {body}");
        assert!(
            body["error"]["details"].to_string().contains(at),
            "{definition} should be refused at {at}: {body}"
        );
    }
    // A key a definition does not define, and the v1 field D10 replaced.
    let (status, _) = send(
        &app,
        "POST",
        DEFINITIONS,
        Some(json!({"kind": "workflow", "definition": {
            "name": "w", "tasks": [], "continue_on_error": true
        }})),
    )
    .await;
    assert!(
        status.is_client_error(),
        "continue_on_error is not a definition field"
    );
}

/// A manifest naming a definition the store does not hold is refused at
/// plan, before anything commits.
#[tokio::test]
async fn a_manifest_naming_a_missing_definition_is_refused() {
    let app = test_app().await;
    let workflow = upload(&app, greeting_workflow()).await;
    let missing = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let (status, body) = send(
        &app,
        "POST",
        PLANS,
        Some(json!({"base_revision": 0, "manifest": manifest(missing, &workflow)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("MEMBER_DEFINITION_NOT_FOUND"),
        "{body}"
    );
}
