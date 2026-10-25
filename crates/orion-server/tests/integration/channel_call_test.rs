use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Create two channels: "source" that calls "target" via channel_call,
/// and verify the end-to-end flow works.
#[tokio::test]
async fn test_channel_call_basic() {
    let app = common::test_app().await;

    // Create target workflow (just logs and maps)
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/admin/workflows",
            Some(json!({
                "name": "Target Workflow",
                "condition": true,
                "tasks": [
                    {
                        "id": "t0",
                        "name": "Parse payload",
                        "function": {
                            "name": "parse_json",
                            "input": { "source": "payload", "target": "input" }
                        }
                    },
                    {
                        "id": "t1",
                        "name": "Map result",
                        "function": {
                            "name": "map",
                            "input": {
                                "mappings": [{
                                    "path": "data.greeting",
                                    "logic": { "cat": ["Hello, ", { "var": "data.input.name" }] }
                                }]
                            }
                        }
                    }
                ]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let target_wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate target workflow
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v2/admin/workflows/{}/status", target_wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create target channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/admin/channels",
            Some(json!({
                "name": "target",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/target",
                "workflow_id": target_wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let target_ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate target channel
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v2/admin/channels/{}/status", target_ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create source workflow that calls the target channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/admin/workflows",
            Some(json!({
                "name": "Source Workflow",
                "condition": true,
                "tasks": [{
                    "id": "s1",
                    "name": "Call target channel",
                    "function": {
                        "name": "channel_call",
                        "input": {
                            "channel": "target",
                            "output": "data.target_result"
                        }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let source_wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Activate source workflow
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v2/admin/workflows/{}/status", source_wf_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Create source channel
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/admin/channels",
            Some(json!({
                "name": "source",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/source",
                "workflow_id": source_wf_id,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = common::body_json(resp).await;
    let source_ch_id = body["data"]["channel_id"].as_str().unwrap().to_string();

    // Activate source channel
    app.clone()
        .oneshot(common::json_request(
            "PATCH",
            &format!("/api/v2/admin/channels/{}/status", source_ch_id),
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();

    // Now call the source channel — it should invoke target via channel_call
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/source",
            Some(json!({"data": {"name": "World"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    // The target workflow maps "Hello, World" into data.greeting
    // and channel_call stores the child's data at data.target_result
    assert_eq!(body["data"]["target_result"]["greeting"], "Hello, World");
}

/// channel_call with a non-existent target channel should return an error.
#[tokio::test]
async fn test_channel_call_missing_target() {
    let app = common::test_app().await;

    // Create workflow that calls a non-existent channel
    common::create_and_activate_channel(
        &app,
        "caller",
        json!({
            "name": "Caller Workflow",
            "condition": true,
            "tasks": [{
                "id": "c1",
                "name": "Call missing",
                "function": {
                    "name": "channel_call",
                    "input": {
                        "channel": "nonexistent",
                        // A path: `"result"` names no context root and is
                        // refused at load (O10's named break, pinned by
                        // `channel_load_refusal_test::
                        // an_output_that_is_no_path_quarantines_its_channel`).
                        "output": "data.result"
                    }
                }
            }]
        }),
    )
    .await;

    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/caller",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();

    // A missing target must fail the request, not silently no-op:
    // `process_message_for_channel` on an unknown channel matches zero
    // workflows and reports success, so without the handler's explicit
    // refusal a typo'd channel name would "work". The data plane sanitizes
    // engine errors (G1), so the client sees the generic ENGINE_ERROR
    // envelope; the message naming the channel is in the logs and trace.
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "ENGINE_ERROR");
}

/// A target whose workflow fails its tasks must fail the caller.
///
/// The engine answers `Ok(())` even when individual workflows failed — the
/// failures go into `message.errors()`, the "v3 contract". Every transport
/// derives its outcome from that; `channel_call` did not, because it read only
/// the outer `Result`. So a target that errored reported success and the
/// caller merged whatever half-finished `data` it had left, with nothing in
/// the response saying so. The shared post-admission step derives it once, for
/// every transport.
#[tokio::test]
async fn a_target_whose_workflow_errors_fails_the_caller() {
    let app = common::test_app().await;

    // The target verifies a token that is not a JWT: valid at activation (no
    // connector, a well-formed task), a task error at run time. A connector
    // that does not exist would not do — activation refuses that (F52).
    common::create_and_activate_channel(
        &app,
        "failing-target",
        json!({
            "name": "Failing Target",
            "condition": true,
            // The engine reports a hard task failure through the outer
            // `Result`; `continue_on_error` is what produces the shape this
            // test is about — `Ok(())` with the failures in
            // `message.errors()`, the v3 contract `channel_call` ignored.
            "continue_on_error": true,
            "tasks": [{
                "id": "boom",
                "name": "Verify a token that cannot be verified",
                "function": {
                    "name": "jwt_verify",
                    "input": {
                        "token": "not-a-jwt",
                        "algorithms": ["HS256"],
                        "keys": [{
                            "algorithm": "HS256",
                            "key": "a-test-secret-at-least-32-bytes-long"
                        }],
                        "output": "data.claims"
                    }
                }
            }]
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "errors-caller",
        json!({
            "name": "Caller Workflow",
            "condition": true,
            "tasks": [{
                "id": "c1",
                "name": "Call the failing target",
                "function": {
                    "name": "channel_call",
                    "input": {
                        "channel": "failing-target",
                        "output": "data.result"
                    }
                }
            }]
        }),
    )
    .await;

    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v2/data/errors-caller",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();

    // The caller's own task failed, so the request fails rather than
    // answering `200 {"data":{"result":{}},"errors":[]}` — which is exactly
    // what it did before the outcome was derived in one place. The data plane
    // sanitizes engine errors (G1), so the client sees the generic envelope;
    // the message naming the target and its failures is in the log and trace.
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a target whose workflow errored must not report success: {body}"
    );
    assert_eq!(body["error"]["code"], "ENGINE_ERROR");
}

// ============================================================
// Recursion guards: cycle detection and max call depth
// ============================================================
//
// The guards live in ChannelCallHandler (max_call_depth / _orion_call_chain).
// When a guard fires below the entry level, the child's error is wrapped in a
// FunctionExecution error and the data plane sanitizes it (G1) — the client
// sees a generic 500 ENGINE_ERROR. The end-to-end tests therefore pin "the
// request fails instead of recursing"; the entry-level tests below inject
// call metadata so the guard fires on the first hop, where the Validation
// message survives to the client as a 400 and the specific refusal text can
// be asserted.

/// A workflow whose single task channel_calls `target`.
fn calls_channel_workflow(name: &str, target: &str) -> serde_json::Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [{
            "id": "call",
            "name": "Call next channel",
            "function": {
                "name": "channel_call",
                "input": { "channel": target, "output": "data.child" }
            }
        }]
    })
}

/// D6: a cron channel is not callable. Its workflow is meant to run once per
/// occurrence, recorded in the ledger and serialised by its singleton key. A
/// `channel_call` would run it with none of that — no occurrence row, and no
/// lock, so a caller could overlap a `forbid` schedule with itself at will.
///
/// The refusal is a task failure rather than a guard refusal, because it is
/// about what the target *is* rather than about which guards this transport
/// applies.
#[tokio::test]
async fn a_cron_channel_is_not_callable() {
    let app = common::test_app().await;

    // The target: an active cron channel.
    let target_wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Scheduled Work"))
            .await;
    common::create_cron_channel(
        &app,
        "scheduled-target",
        &target_wf,
        json!({"schedule": "0 15 2 * * *"}),
    )
    .await;

    // The caller: an ordinary channel whose workflow calls it.
    common::create_and_activate_channel(
        &app,
        "cron-caller",
        calls_channel_workflow("Cron Caller", "scheduled-target"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/cron-caller",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "calling a cron channel must fail the task, not run it"
    );
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "ENGINE_ERROR");
}

/// A channel whose workflow calls itself must be refused by cycle detection,
/// not recurse until the stack or the engine gives out. The entry channel is
/// not part of the recorded chain, so the cycle is caught on the second hop:
/// loop-self -> loop-self.
#[tokio::test]
async fn self_call_is_refused_as_a_cycle() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "loop-self",
        calls_channel_workflow("Self Loop", "loop-self"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/loop-self",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    // Refused — not a 200 silent success, not a hang. The child's refusal is
    // passed on with its own code, so this answers as the entry-level refusal
    // in injected_call_chain_surfaces_cycle_refusal_message does.
    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "LOOP_DETECTED");
}

/// A -> B -> A must be refused by cycle detection. The chain records call
/// targets only, so the cycle surfaces when A's task runs a second time:
/// cycle-b -> cycle-a -> cycle-b.
#[tokio::test]
async fn mutual_recursion_is_refused_as_a_cycle() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "cycle-a",
        calls_channel_workflow("Cycle A", "cycle-b"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "cycle-b",
        calls_channel_workflow("Cycle B", "cycle-a"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/cycle-a",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "LOOP_DETECTED");
}

/// A linear chain of distinct channels deeper than `max_channel_call_depth`
/// must be cut off by the depth guard. Distinct names keep cycle detection
/// out of the way, and every link targets an existing channel, so a guard
/// regression shows up as a 200 walk straight through the chain rather than
/// as some other error.
#[tokio::test]
async fn call_depth_beyond_configured_max_is_refused() {
    let app = common::test_app_with_config(orion::config::AppConfig {
        engine: orion::config::EngineConfig {
            max_channel_call_depth: 2,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    common::create_and_activate_channel(
        &app,
        "chain-end",
        common::simple_log_workflow("Chain End"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "chain-3",
        calls_channel_workflow("Chain 3", "chain-end"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "chain-2",
        calls_channel_workflow("Chain 2", "chain-3"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "chain-1",
        calls_channel_workflow("Chain 1", "chain-2"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/chain-1",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();

    // chain-3's call runs at depth 2 and must be refused; the refusal is
    // passed up the chain with its code. If the guard regressed, the chain
    // completes and this returns 200 ok.
    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "LOOP_DETECTED");
}

/// Entry-level cycle refusal: caller-supplied `metadata` is merged into the
/// message (documented data-plane behaviour), so a request arriving with the
/// target already in `_orion_call_chain` trips the guard on the first hop.
/// There the task's Validation error reaches the client unwrapped — a 400
/// whose message pins the "cycle detected" refusal text and chain rendering.
#[tokio::test]
async fn injected_call_chain_surfaces_cycle_refusal_message() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "probe-target",
        common::simple_log_workflow("Probe Target"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "probe-entry",
        calls_channel_workflow("Probe Entry", "probe-target"),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/probe-entry",
            Some(json!({
                "data": {},
                "metadata": { "_orion_call_chain": ["probe-target"], "_orion_call_depth": 1 }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "LOOP_DETECTED");
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("cycle detected") && message.contains("probe-target -> probe-target"),
        "expected the cycle refusal with the rendered chain, got: {message}"
    );
}

/// Entry-level depth refusal: a request arriving already at the configured
/// max depth is refused before any call is made, with the limit named in
/// the message.
#[tokio::test]
async fn injected_call_depth_surfaces_depth_refusal_message() {
    let app = common::test_app().await;
    common::create_and_activate_channel(
        &app,
        "depth-target",
        common::simple_log_workflow("Depth Target"),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "depth-entry",
        calls_channel_workflow("Depth Entry", "depth-target"),
    )
    .await;

    // Default max_channel_call_depth is 10.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/depth-entry",
            Some(json!({
                "data": {},
                "metadata": { "_orion_call_depth": 10 }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"]["code"], "LOOP_DETECTED");
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("max call depth 10 exceeded"),
        "expected the depth refusal naming the limit, got: {message}"
    );
}

// ============================================================
// HTTP Call End-to-End with Mock Server
// ============================================================

#[tokio::test]
async fn test_http_call_end_to_end() {
    let mock_app = axum::Router::new().route(
        "/api/users",
        axum::routing::post(|| async {
            axum::Json(json!({"user_id": "123", "status": "created"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;

    common::create_http_connector(&app, "mock-http-api", mock_addr).await;

    common::create_and_activate_channel(
        &app,
        "http-call-ch",
        json!({
            "name": "HTTP Call Integration",
            "condition": true,
            "tasks": [{
                "id": "call-api",
                "name": "Call Mock API",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "mock-http-api",
                        "method": "POST",
                        "path": "/api/users",
                        "body": {"test": true},
                        "output": "data.api_response",
                        "timeout_ms": 5000
                    }
                }
            }]
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/http-call-ch",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    // The mock's response must land at the configured output — this
    // is the thing the mock server exists to prove.
    assert_eq!(body["data"]["api_response"]["user_id"], "123");
    assert_eq!(body["data"]["api_response"]["status"], "created");
}

/// A computed `channel` and `data` both work end-to-end.
///
/// Neither had any coverage, which mattered when they moved from raw
/// `serde_json::Value` — recompiled from JSON on **every message**, the only
/// two `ctx.datalogic().compile(..)` calls left in the handler surface — to
/// a compiled template, compiled once at engine construction. The observable contract is unchanged; this pins it.
#[tokio::test]
async fn channel_call_resolves_target_and_payload_from_logic() {
    let app = common::test_app().await;

    // Two leaves, so a dynamic target has something to choose between.
    for leaf in ["alpha", "beta"] {
        common::create_and_activate_channel(
            &app,
            leaf,
            json!({
                "name": format!("Leaf {leaf}"),
                "condition": true,
                "tasks": [{
                    "id": "t0",
                    "name": "Parse payload",
                    "function": {"name": "parse_json", "input": {"source": "payload", "target": "input"}}
                }, {
                    "id": "t1",
                    "name": "Echo which leaf ran, and what it was sent",
                    "function": {
                        "name": "map",
                        "input": {"mappings": [
                            {"path": "data.served_by", "logic": leaf},
                            {"path": "data.saw", "logic": {"var": "data.input.forwarded"}}
                        ]}
                    }
                }]
            }),
        )
        .await;
    }

    common::create_and_activate_channel(
        &app,
        "dispatcher",
        json!({
            "name": "Dispatcher",
            "condition": true,
            "tasks": [
                {
                    "id": "parse",
                    "name": "Parse payload",
                    "function": {"name": "parse_json", "input": {"source": "payload", "target": "input"}}
                },
                {
                    "id": "call",
                    "name": "Call whichever leaf the request names",
                    "function": {
                        "name": "channel_call",
                        "input": {
                            "channel": {"var": "data.input.route"},
                            "data": {"forwarded": {"var": "data.input.token"}},
                            "output": "data.child"
                        }
                    }
                }
            ]
        }),
    )
    .await;

    for route in ["alpha", "beta"] {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/dispatcher",
                Some(json!({"data": {"route": route, "token": format!("t-{route}")}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body["data"]["child"]["served_by"], route,
            "a computed channel must pick the target per message, got: {body}"
        );
        assert_eq!(
            body["data"]["child"]["saw"],
            format!("t-{route}"),
            "a computed data must build the payload per message, got: {body}"
        );
    }
}

// -- channel_call on the published generation (O11a) --

/// What a caller's run left: the `data` it wrote, or its first error
/// record's code and message.
type Seen = Result<serde_json::Value, (String, String)>;

/// `channel` run on the generation's engine.
async fn seen_on(generation: &orion::runtime::RuntimeGeneration, channel: &str) -> Seen {
    use dataflow_rs::datavalue::OwnedDataValue;
    let engine4 = &generation.engine;
    let mut message = dataflow_rs::Message::from_parts(
        OwnedDataValue::from(&json!({})),
        OwnedDataValue::Null,
        OwnedDataValue::from(&json!({})),
    );
    let options = dataflow_rs::RunOptions::default()
        .channel(channel)
        .capture_changes(false)
        .audit(dataflow_rs::run::AuditMode::Off);
    let _ = engine4.run_with(&mut message, &options).await;
    match message.errors().first() {
        Some(record) => Err((record.code.clone(), record.message.clone())),
        None => Ok(serde_json::Value::from(message.context().data())),
    }
}

/// `channel_call` through the generation a deployment publishes (O10,
/// O11a):
///
/// - a call into a channel the generation serves runs its target on the
///   generation's engine;
/// - a target whose workflow would not serve (a typo'd key) never reaches a
///   generation: its definition is refused at upload, naming the key;
/// - a call into no channel is refused as not found or not active.
#[tokio::test]
async fn a_channel_call_runs_on_the_generations_engine() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let workflow = |name: &str, tasks: serde_json::Value| json!({"name": name, "condition": true, "tasks": tasks});
    let call = |target: &str| {
        json!([{"id": "c1", "name": "call", "function": {"name": "channel_call",
            "input": {"channel": target, "data": {"x": 1}, "output": "data.child"}}}])
    };
    common::create_and_activate_channel(
        &app,
        "v4-target",
        workflow(
            "Target",
            json!([{"id": "t1", "name": "greet", "function": {"name": "map", "input": {
                "mappings": [{"path": "data.greeting", "logic": "hello"}]}}}]),
        ),
    )
    .await;
    // Break 2: `mesage` is a near miss of `log`'s `message`.
    let (status, refused) = common::deploy::try_upload(
        &app,
        common::deploy::workflow_definition(&workflow(
            "Broken",
            json!([{"id": "t1", "name": "log", "function": {"name": "log", "input": {
                "message": "x", "mesage": "y"}}}]),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        common::deploy::refused_paths(&refused)
            .contains(&"definition.tasks[0].function.input.mesage".to_string()),
        "the refusal names the typo'd key: {refused}"
    );
    for (caller, target) in [
        ("v4-caller", "v4-target"),
        ("v4-caller-missing", "v4-no-such-channel"),
    ] {
        common::create_and_activate_channel(&app, caller, workflow(caller, call(target))).await;
    }

    let generation = state.runtime.load();
    let served = seen_on(&generation, "v4-caller").await;
    assert_eq!(served, Ok(json!({"child": {"greeting": "hello"}})));

    let missing = seen_on(&generation, "v4-caller-missing").await;
    let (_, message) = missing.as_ref().expect_err("no target is refused");
    assert!(
        message.contains("channel not found or not active"),
        "{message}"
    );
}
