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
        Some(
            crate::common::deploy::reviewed(
                &app,
                json!({
                    "idempotency_key": "greet-1",
                    "base_revision": 0,
                    "manifest": manifest(&channel, &workflow),
                    "reason": "deploy greet",
                }),
            )
            .await,
        ),
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

/// A channel reads back with its credentials masked — references kept — while
/// the digest stays the stored, unmasked definition's.
#[tokio::test]
async fn a_channel_reads_back_with_its_credentials_masked() {
    let app = test_app().await;
    let channel = json!({"kind": "channel", "definition": {
        "name": "keyed",
        "protocol": "rest",
        "methods": ["POST"],
        "route_pattern": "/keyed",
        "config": {"auth": {"mode": "api_key", "keys": ["sk_live_one", "env://SECOND_KEY"]}}
    }});
    let digest = upload(&app, channel.clone()).await;
    let (status, stored) = send(&app, "GET", &format!("{DEFINITIONS}/{digest}"), None).await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(
        stored["definition"]["config"]["auth"]["keys"],
        json!(["******", "env://SECOND_KEY"]),
        "{stored}"
    );
    assert!(!stored.to_string().contains("sk_live_one"), "{stored}");
    // The same definition uploaded again is still the same digest: masking
    // is the read's, not the store's.
    assert_eq!(upload(&app, channel).await, digest);
}

/// A workflow upload answers what the engine advises and does not refuse,
/// and stores the workflow.
#[tokio::test]
async fn a_workflow_upload_reports_its_advisories() {
    let app = test_app().await;
    let (status, body) = send(
        &app,
        "POST",
        DEFINITIONS,
        Some(json!({"kind": "workflow", "definition": {
            "name": "Check",
            "tasks": [
                {"id": "check", "function": {"name": "validation", "input": {
                    "rules": [{"logic": {"==": [1, 2]}, "message": "no"}]}}},
                {"id": "respond", "function": {"name": "map", "input": {
                    "mappings": [{"path": "data.x", "logic": true}]}}}
            ]
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let advisories = body["advisories"].as_array().expect("an advisories list");
    assert!(
        advisories
            .iter()
            .any(|a| a["check"] == "engine.unguarded_validation"),
        "{body}"
    );
    assert!(
        advisories
            .iter()
            .all(|a| a["message"].is_string() && a["path"].is_string()),
        "{body}"
    );

    let (status, body) = send(&app, "POST", DEFINITIONS, Some(greeting_workflow())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["advisories"], json!([]), "{body}");
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

/// A Connection over a built-in driver is a connector the built-in functions
/// use: `db_read` reads through a deployed `org.orion.sqlite` connection by
/// its id, with no v1 connector anywhere.
#[tokio::test]
async fn a_deployed_connection_serves_the_built_in_functions() {
    let app = test_app().await;
    let sqlite = orion::test_support::builtin_provider_digest(orion::test_support::DRIVER_SQLITE)
        .expect("a built-in driver");
    let (status, created) = send(
        &app,
        "POST",
        "/api/v2/admin/connections/numbers/revisions",
        Some(json!({
            "revision": 1,
            "provider_digest": sqlite,
            "public_config": {"connection_string": "sqlite::memory:"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let connection = created["connection"]["configuration_digest"]
        .as_str()
        .expect("a digest")
        .to_string();

    let workflow = upload(
        &app,
        json!({"kind": "workflow", "definition": {
            "name": "Count",
            "tasks": [{"id": "read", "function": {"name": "db_read", "input": {
                "connector": "numbers",
                "query": "SELECT 41 + 1 AS answer",
                "output": "data.rows"
            }}}]
        }}),
    )
    .await;
    let channel = upload(&app, greeting_channel()).await;
    let mut manifest = manifest(&channel, &workflow);
    manifest["members"]
        .as_array_mut()
        .expect("members")
        .push(json!({
            "entity": {"kind": "connection", "id": "numbers", "revision": 1},
            "definition_digest": connection,
        }));
    let (status, applied) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        Some(
            crate::common::deploy::reviewed(
                &app,
                json!({
                    "idempotency_key": "numbers-1",
                    "base_revision": 0,
                    "manifest": manifest,
                    "reason": "deploy numbers",
                }),
            )
            .await,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{applied}");
    assert_eq!(applied["evaluation"]["converged"], true, "{applied}");

    let (status, answer) = send(
        &app,
        "POST",
        "/api/v2/data/greet",
        Some(json!({"data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["data"]["rows"][0]["answer"], 42, "{answer}");
}

/// Two channels of one deployment claiming one route at one priority fail
/// the revision, naming both, and the previous generation keeps serving.
#[tokio::test]
async fn two_channels_claiming_one_route_fail_the_revision() {
    let app = test_app().await;
    let workflow = upload(&app, greeting_workflow()).await;
    let route = |name: &str, pattern: &str| {
        json!({"kind": "channel", "definition": {
            "name": name, "protocol": "rest", "methods": ["GET"], "route_pattern": pattern,
        }})
    };
    let first = upload(&app, route("orders.first", "/orders/{id}")).await;
    // Same shape, another parameter name: one route.
    let second = upload(&app, route("orders.second", "/orders/{order_id}")).await;
    let member = |kind: &str, id: &str, digest: &str| {
        json!({"entity": {"kind": kind, "id": id, "revision": 1},
               "definition_digest": digest,
               "requires": if kind == "channel" {
                   json!([{"kind": "workflow", "id": "wf", "revision": 1}])
               } else { json!([]) }})
    };
    let (status, applied) = send(
        &app,
        "POST",
        DEPLOYMENTS,
        Some(
            crate::common::deploy::reviewed(
                &app,
                json!({
                    "idempotency_key": "collide",
                    "base_revision": 0,
                    "manifest": {"schema_version": 2, "providers": [], "members": [
                        member("workflow", "wf", &workflow),
                        member("channel", "orders.first", &first),
                        member("channel", "orders.second", &second),
                    ]},
                    "reason": "collide",
                }),
            )
            .await,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{applied}");
    assert_eq!(
        applied["error"]["code"], "DEPLOYMENT_WOULD_NOT_BUILD",
        "{applied}"
    );
    let error = applied["error"]["message"].as_str().unwrap_or_default();
    assert!(
        error.contains("orders.first") && error.contains("orders.second"),
        "{applied}"
    );
}

const TEST: &str = "/api/v2/admin/definitions/test";

/// `POST /definitions/test` runs a definition once, deploying nothing, and
/// answers the run with a trace that replays: the context it started from,
/// then each step with the changes it made.
#[tokio::test]
async fn a_definition_test_runs_once_and_answers_a_replayable_trace() {
    let app = test_app().await;
    let (status, body) = send(
        &app,
        "POST",
        TEST,
        Some(json!({
            "definition": greeting_workflow()["definition"],
            "data": {"name": "Ada"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["matched"], true, "{body}");
    assert_eq!(body["output"]["greeting"], "Hello, Ada", "{body}");
    let trace = &body["trace"];
    assert_eq!(trace["initial"]["data"], json!({"name": "Ada"}), "{trace}");
    let executed = trace["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|s| s["result"] == "executed"))
        .unwrap_or_else(|| panic!("an executed step: {body}"));
    assert_eq!(executed["task_id"], "greet");
    assert!(executed["changes"].is_array(), "{executed}");
    assert!(body.get("error").is_none(), "{body}");

    // Nothing was deployed or stored.
    let (_, summary) = send(&app, "GET", DEPLOYMENTS, None).await;
    assert_eq!(
        summary["head_revision"].as_u64().unwrap_or(0),
        0,
        "{summary}"
    );
}

/// A definition the upload would refuse is refused here too, by field, and
/// never runs; so is metadata no ingress could produce.
#[tokio::test]
async fn a_definition_test_refuses_what_would_not_serve() {
    let app = test_app().await;
    let (status, body) = send(
        &app,
        "POST",
        TEST,
        Some(json!({
            "definition": {"name": "Broken", "tasks": [
                {"id": "t", "function": {"name": "no_such_function", "input": {}}}
            ]},
            "data": {},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let paths: Vec<&str> = body["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["path"].as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.starts_with("definition.tasks")),
        "{body}"
    );

    let (status, body) = send(
        &app,
        "POST",
        TEST,
        Some(json!({
            "definition": greeting_workflow()["definition"],
            "data": {},
            "metadata": "not an object",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
