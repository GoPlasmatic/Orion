//! `model_infer` end to end: the fixture model registered, admitted and
//! activated through the admin API, a workflow calling it deployed behind a
//! channel, and the data plane driven through the adapters, the runtime and
//! the result expression — plus every way the call is refused, the
//! quarantine of a workflow naming a model the node cannot serve, and the
//! health the node reports afterwards.
//!
//! Uses the fixture in `tests/fixtures/models/c4-tiny/` (a `[1,2,6,7]` f32
//! board in, a `[1,7]` f32 policy out, 1479 parameters) served by the
//! in-process bucket from `common::models`. Preload is off in the tests
//! that assert `cold_load`, so the first inference is the one that loads.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::models::{
    FIXTURE_ID, ModelHarness, admit, harness_with, models_config, register_fixture,
};
use crate::common::{body_json, json_request};

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, path, body))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

/// A harness with `models.preload` as given and a fresh cache directory.
async fn harness_preloading(preload: orion::config::ModelPreload) -> ModelHarness {
    let mut config = models_config(true);
    config.models.preload = preload;
    harness_with(config).await
}

/// Register the fixture, admit it on this node and activate it.
async fn activate_fixture(h: &ModelHarness) {
    register_fixture(&h.app).await;
    assert!(admit(&h.state, FIXTURE_ID).await.passed());
    let (status, body) = send(
        &h.app,
        "PATCH",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/status"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A `[1, 2, 6, 7]` board as the nested list the fixture adapter reads:
/// 42 cells per plane, a few stones placed.
fn board() -> Value {
    let mut planes = vec![vec![vec![0.0f32; 7]; 6]; 2];
    planes[0][5][3] = 1.0;
    planes[1][5][2] = 1.0;
    planes[0][4][3] = 1.0;
    json!([planes])
}

/// A `model_infer` task over `model`, with `extra` merged into its input.
/// The board arrives as the payload and `parse_json` puts it at
/// `data.board`, which is where the fixture manifest's adapter reads it
/// from when the task hands it the whole context.
fn tasks(model: Value, extra: Value) -> Value {
    let mut input = json!({
        "model": model,
        "input": {"var": ""},
        "output": "data.policy",
    });
    if let (Some(into), Some(from)) = (input.as_object_mut(), extra.as_object()) {
        for (k, v) in from {
            into.insert(k.clone(), v.clone());
        }
    }
    json!([
        {"id": "parse", "name": "parse", "function": {"name": "parse_json",
            "input": {"source": "payload", "target": "board"}}},
        {"id": "infer", "name": "infer", "function": {"name": "model_infer", "input": input}}
    ])
}

/// Create and activate a workflow over `tasks` and a channel routing to it;
/// hand back the channel name.
async fn deploy(app: &axum::Router, name: &str, tasks: Value) -> String {
    let (status, body) = send(
        app,
        "POST",
        "/api/v1/admin/workflows",
        Some(json!({"name": name, "condition": true, "tasks": tasks})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let workflow_id = body["data"]["workflow_id"]
        .as_str()
        .expect("workflow id")
        .to_string();
    let (status, body) = send(
        app,
        "PATCH",
        &format!("/api/v1/admin/workflows/{workflow_id}/status"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app,
        "POST",
        "/api/v1/admin/channels",
        Some(json!({
            "name": name,
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": format!("/{name}"),
            "workflow_id": workflow_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let channel_id = body["data"]["channel_id"]
        .as_str()
        .expect("channel id")
        .to_string();
    let (status, body) = send(
        app,
        "PATCH",
        &format!("/api/v1/admin/channels/{channel_id}/status"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    name.to_string()
}

async fn call(app: &axum::Router, channel: &str, payload: Value) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        &format!("/api/v1/data/{channel}"),
        Some(json!({"data": payload})),
    )
    .await
}

fn cleanup(h: &ModelHarness) {
    if let Some(models) = h.state.models.as_ref() {
        let _ = std::fs::remove_dir_all(models.store.cache_dir());
    }
}

/// The whole path: the result expression's shape, the stats, the cold and
/// warm calls, and what the node reports about the model afterwards.
#[tokio::test]
async fn an_inference_runs_end_to_end_and_the_node_reports_it_loaded() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    activate_fixture(&h).await;
    let channel = deploy(
        &h.app,
        "infer",
        tasks(json!(FIXTURE_ID), json!({"stats_output": "data.stats"})),
    )
    .await;

    // Nothing is resident before the first call.
    let (_, body) = send(
        &h.app,
        "GET",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        None,
    )
    .await;
    assert_eq!(body["data"]["health"]["state"], "admitted", "{body}");

    let started = Instant::now();
    let (status, body) = call(&h.app, &channel, board()).await;
    let first_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The fixture's result: `{"policy": to_list(policy)}` over a `[1, 7]`
    // tensor is one row of seven numbers.
    let policy = body["data"]["policy"]["policy"]
        .as_array()
        .expect("a list of rows");
    assert_eq!(policy.len(), 1, "{body}");
    let row = policy[0].as_array().expect("a row of seven");
    assert_eq!(row.len(), 7, "{body}");
    assert!(row.iter().all(Value::is_number), "{body}");
    let stats = &body["data"]["stats"];
    assert_eq!(stats["id"], FIXTURE_ID, "{stats}");
    assert_eq!(stats["version"], 1, "{stats}");
    assert_eq!(stats["runtime"], "tract", "{stats}");
    assert_eq!(stats["device"], "cpu", "{stats}");
    assert_eq!(stats["parameters"], 1479, "{stats}");
    assert!(stats["artifact_bytes"].as_u64().unwrap_or(0) > 0, "{stats}");
    assert_eq!(stats["cold_load"], true, "{stats}");
    assert!(stats["inference_ms"].is_number() && stats["queued_ms"].is_number());
    assert!(
        stats["digest"]
            .as_str()
            .unwrap_or("")
            .starts_with("sha256:")
    );

    let started = Instant::now();
    let (status, body) = call(&h.app, &channel, board()).await;
    let second_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["stats"]["cold_load"], false, "{body}");
    println!("model_infer end to end: cold {first_ms:.1}ms, warm {second_ms:.1}ms");

    // The node's view: resident, on the runtime and device it loaded on.
    let (_, body) = send(
        &h.app,
        "GET",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        None,
    )
    .await;
    let health = &body["data"]["health"];
    assert_eq!(health["state"], "loaded", "{body}");
    assert_eq!(health["runtime"], "tract", "{body}");
    assert_eq!(health["device"], "cpu", "{body}");
    assert!(health["resident_bytes"].as_u64().unwrap_or(0) > 0, "{body}");

    let (status, body) = send(&h.app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"]["models"], "ok", "{body}");
    assert!(
        body["models"]["loaded_bytes"].as_u64().unwrap_or(0) > 0,
        "{body}"
    );
    assert_eq!(body["models"]["loaded"][0]["runtime"], "tract", "{body}");
    assert_eq!(body["models"]["failed_to_load"], json!([]), "{body}");
    cleanup(&h);
}

/// `raw: true` writes the outputs as tagged tensors, in wire form, for a
/// later task to chain on.
#[tokio::test]
async fn raw_writes_the_tagged_tensor() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    activate_fixture(&h).await;
    let channel = deploy(
        &h.app,
        "raw",
        tasks(
            json!(FIXTURE_ID),
            json!({"raw": true, "output": "data.out"}),
        ),
    )
    .await;
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tensor = &body["data"]["out"]["policy"]["tensor"];
    assert_eq!(tensor["dtype"], "f32", "{body}");
    assert_eq!(tensor["shape"], json!([1, 7]), "{body}");
    assert!(tensor["data"].is_string(), "{body}");
    // The default output path applies when none is named.
    let channel = deploy(&h.app, "raw-default", {
        let mut t = tasks(json!(FIXTURE_ID), json!({"raw": true}));
        t[1]["function"]["input"]
            .as_object_mut()
            .expect("object")
            .remove("output");
        t.as_array_mut().expect("tasks").push(json!({
            "id": "lift", "name": "lift", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.shape", "logic": {"shape": [{"var": "temp_data.inference.policy"}]}}
            ]}}
        }));
        t
    })
    .await;
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["shape"], json!([1, 7]), "{body}");
    cleanup(&h);
}

/// A computed `model` routes per message: the id the message names runs,
/// and one the node does not serve fails the task as unavailable.
#[tokio::test]
async fn a_computed_model_routes_per_message_and_an_unknown_one_is_unavailable() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    activate_fixture(&h).await;
    let mut t = tasks(json!({"var": "data.which"}), json!({}));
    // The board and the model id both ride in the payload.
    t[0]["function"]["input"]["target"] = json!("input");
    t.as_array_mut().expect("tasks").insert(
        1,
        json!({"id": "lift", "name": "lift", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.board", "logic": {"var": "data.input.board"}},
            {"path": "data.which", "logic": {"var": "data.input.model"}}
        ]}}}),
    );
    let channel = deploy(&h.app, "routed", t).await;
    let (status, body) = call(
        &h.app,
        &channel,
        json!({"board": board(), "model": FIXTURE_ID}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["policy"]["policy"][0].as_array().map(Vec::len),
        Some(7)
    );

    // An id the node does not serve fails the task as `unavailable` — a
    // backend-class failure, so the data plane answers a redacted 500 (G1);
    // the message naming the model is the operator's, on the log and the
    // trace, and `model::handler`'s own tests pin its text.
    let (status, body) = call(
        &h.app,
        &channel,
        json!({"board": board(), "model": "ada.nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "ENGINE_ERROR", "{body}");
    assert!(body.get("data").is_none(), "{body}");
    cleanup(&h);
}

/// A `runtime` this build does not know is refused when the workflow is
/// written, by code.
#[tokio::test]
async fn an_unknown_runtime_is_refused_at_create_time() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/workflows",
        Some(json!({
            "name": "bad runtime",
            "condition": true,
            "tasks": tasks(json!(FIXTURE_ID), json!({"runtime": "nope"}))
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let details = body["error"]["details"].as_array().expect("details");
    assert!(
        details.iter().any(|d| d["code"] == "MODEL_RUNTIME_UNKNOWN"
            && d["path"].as_str().unwrap_or("").ends_with(".runtime")
            && d["message"].as_str().unwrap_or("").contains("tract")),
        "{body}"
    );
    cleanup(&h);
}

/// The adapters are JSONLogic under `engine.ops_budget`: a ceiling below
/// the cost of building the board tensor refuses the call with the
/// budget's own refusal.
#[tokio::test]
async fn the_ops_budget_prices_the_adapter() {
    let mut config = models_config(true);
    config.models.preload = orion::config::ModelPreload::None;
    config.engine.ops_budget = 20;
    let h = harness_with(config).await;
    activate_fixture(&h).await;
    let channel = deploy(&h.app, "budget", tasks(json!(FIXTURE_ID), json!({}))).await;
    let (status, body) = call(&h.app, &channel, board()).await;
    // A limit the caller can read: the refusal's own text, as a 400.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("model_infer")
            && message.contains("adapter for input 'board'")
            && message.contains("budget of 20"),
        "{body}"
    );
    assert!(body.get("data").is_none(), "{body}");
    cleanup(&h);
}

/// A workflow naming, by literal id, a model this node cannot serve is
/// quarantined with the reason — while the model is only registered, and
/// again when an active version's admission is later recorded as failed.
#[tokio::test]
async fn a_literal_reference_to_an_unserved_model_quarantines_the_channel() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    // Registered, pending admission, a draft: no active version serves.
    register_fixture(&h.app).await;
    let channel = deploy(&h.app, "quarantined", tasks(json!(FIXTURE_ID), json!({}))).await;
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (_, health) = send(&h.app, "GET", "/health", None).await;
    assert_eq!(health["components"]["channels"], "degraded", "{health}");
    let quarantined = health["channels"]["quarantined"].as_array().expect("array");
    let issue = quarantined
        .iter()
        .find(|q| q["channel"] == channel)
        .expect("the channel is listed");
    let reason = issue["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("task 'infer'")
            && reason.contains(&format!("model '{FIXTURE_ID}'"))
            && reason.contains("no active version is admitted"),
        "{reason}"
    );
    assert_eq!(health["components"]["models"], "ok", "{health}");

    // Admitted and activated, the reload lifts the quarantine.
    assert!(admit(&h.state, FIXTURE_ID).await.passed());
    let (status, body) = send(
        &h.app,
        "PATCH",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/status"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The active row's verdict rewritten as failed — what a peer's
    // re-admission against a broken bucket records — and the next reload
    // quarantines again, naming the stage and the reason, and the model
    // component degrades.
    let node = h.state.models.as_ref().expect("enabled").node.clone();
    h.state
        .repos
        .models
        .set_admission(
            FIXTURE_ID,
            1,
            &json!({"state": "failed", "stage": "fetch", "reason": "GET answered HTTP 503",
                "node": node, "at": "2026-09-13T00:00:00"})
            .to_string(),
        )
        .await
        .expect("record");
    let (status, body) = send(&h.app, "POST", "/api/v1/admin/engine/reload", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (_, health) = send(&h.app, "GET", "/health", None).await;
    assert_eq!(health["components"]["models"], "degraded", "{health}");
    let failed = &health["models"]["failed_to_load"][0];
    assert_eq!(failed["model"], FIXTURE_ID, "{health}");
    assert_eq!(failed["stage"], "admission", "{health}");
    assert!(
        failed["reason"]
            .as_str()
            .unwrap_or("")
            .contains("GET answered HTTP 503"),
        "{health}"
    );
    let reason = health["channels"]["quarantined"]
        .as_array()
        .expect("array")
        .iter()
        .find(|q| q["channel"] == channel)
        .and_then(|q| q["reason"].as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("admission: admission is 'failed' at stage 'fetch'"),
        "{reason}"
    );
    let (_, body) = send(
        &h.app,
        "GET",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        None,
    )
    .await;
    assert_eq!(body["data"]["health"]["state"], "rejected", "{body}");
    cleanup(&h);
}

/// `models.preload = "referenced"` warms a model an active workflow names
/// after the publish, so the first inference finds it resident.
#[tokio::test]
async fn the_referenced_preload_warms_the_model_before_the_first_call() {
    let h = harness_preloading(orion::config::ModelPreload::Referenced).await;
    activate_fixture(&h).await;
    let channel = deploy(
        &h.app,
        "warm",
        tasks(json!(FIXTURE_ID), json!({"stats_output": "data.stats"})),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = send(
            &h.app,
            "GET",
            &format!("/api/v1/admin/models/{FIXTURE_ID}"),
            None,
        )
        .await;
        if body["data"]["health"]["state"] == "loaded" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the preload never loaded the model: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, body) = call(&h.app, &channel, board()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["stats"]["cold_load"], false, "{body}");
    cleanup(&h);
}

/// Every way the call itself is refused before the model runs, each with a
/// message the caller can act on.
#[tokio::test]
async fn caller_input_that_does_not_marshal_is_refused_without_a_load() {
    let h = harness_preloading(orion::config::ModelPreload::None).await;
    activate_fixture(&h).await;
    let channel = deploy(&h.app, "shape", tasks(json!(FIXTURE_ID), json!({}))).await;
    // A board of the wrong shape: the adapter builds a tensor, but not the
    // declared one.
    let (status, body) = call(&h.app, &channel, json!([[[0.0, 1.0]]])).await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("input 'board'") && text.contains("expected f32[1,2,6,7]"),
        "{text}"
    );
    // Nothing was loaded for a call that could not marshal.
    let (_, body) = send(
        &h.app,
        "GET",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        None,
    )
    .await;
    assert_eq!(body["data"]["health"]["state"], "admitted", "{body}");
    cleanup(&h);
}
