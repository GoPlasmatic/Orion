//! A2 (Tier-1 ergonomics): per-task trace data (intermediate inputs/outputs).
//!
//! Verifies that:
//!   - the dry-run `/test` endpoint already returns per-step `message` snapshots
//!     from `dataflow_rs::ExecutionTrace` (no code change — locks in the behavior)
//!   - opting into `config.tracing.task_details = true` causes the engine to
//!     capture an `ExecutionTrace` and persist it as `task_trace_json` on the
//!     resulting trace row

mod common;

use axum::http::StatusCode;
use common::{
    body_json, create_and_activate_channel_with_config, create_and_activate_workflow, json_request,
    test_app,
};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn dry_run_test_endpoint_returns_per_step_message_snapshots() {
    let app = test_app().await;
    // Create a draft workflow with a single log task (avoids needing connectors).
    let create_resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "trace-shape",
                "tasks": [{
                    "id": "t1",
                    "name": "log it",
                    "function": { "name": "log", "input": { "message": "hello" } }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);
    let body = body_json(create_resp).await;
    let wf_id = body["data"]["workflow_id"].as_str().unwrap().to_string();

    // Dry-run it.
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/workflows/{}/test", wf_id),
            Some(json!({ "data": { "x": 1 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // dataflow_rs::ExecutionTrace exposes steps[] with per-step task_id +
    // message snapshot. Confirm the shape so workflow authors can rely on it.
    let steps = body["trace"]["steps"]
        .as_array()
        .expect("trace.steps must be an array");
    assert!(
        !steps.is_empty(),
        "expected at least one executed step, got {body:?}"
    );
    let executed = steps
        .iter()
        .find(|s| s["result"] == "executed")
        .expect("expected at least one executed step");
    assert_eq!(executed["task_id"], "t1");
    // The full per-step message snapshot is included — this is what makes
    // multi-step workflows debuggable.
    assert!(
        executed.get("message").is_some(),
        "executed step must include `message` snapshot, got {executed:?}"
    );
}

#[tokio::test]
async fn sync_request_with_task_details_persists_task_trace_json() {
    let app = test_app().await;

    let wf_id = create_and_activate_workflow(
        &app,
        json!({
            "name": "task-trace-wf",
            "tasks": [{
                "id": "t1",
                "name": "log",
                "function": { "name": "log", "input": { "message": "ping" } }
            }]
        }),
    )
    .await;

    // Channel with `tracing.task_details = true` — opt in to per-task capture.
    let (channel_name, _) = create_and_activate_channel_with_config(
        &app,
        "task-trace-channel",
        json!({
            "name": "task-trace-wf-clone",
            "tasks": [{
                "id": "t1",
                "name": "log",
                "function": { "name": "log", "input": { "message": "ping" } }
            }]
        }),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;
    // Override the channel's workflow to the activated one (the helper
    // creates a fresh workflow; we want our active one).
    let _ = wf_id; // not used further — channel routes via its own workflow_id

    // POST data to the channel.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}", channel_name),
            Some(json!({ "data": { "x": 1 } })),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "POST failed: {body:?}");

    // Allow the trace persistence (sync mode) to settle.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // List traces and find the row from this request.
    let resp = app
        .oneshot(json_request("GET", "/api/v1/data/traces", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let row = body["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == channel_name))
        .expect("expected at least one trace row for the task-trace channel");
    // The captured ExecutionTrace must be persisted as JSON in `task_trace_json`.
    let task_trace_json = row["task_trace_json"]
        .as_str()
        .expect("task_trace_json must be populated when task_details=true");
    let parsed: serde_json::Value = serde_json::from_str(task_trace_json).unwrap();
    let steps = parsed["steps"]
        .as_array()
        .expect("persisted task_trace_json must include steps[]");
    assert!(!steps.is_empty());
    // At least one step must reference the task we configured.
    assert!(steps.iter().any(|s| s["task_id"] == "t1"));
}

#[tokio::test]
async fn sync_request_without_task_details_omits_task_trace_json() {
    let app = test_app().await;

    let (channel_name, _) = create_and_activate_channel_with_config(
        &app,
        "no-task-trace-channel",
        json!({
            "name": "no-task-trace-wf",
            "tasks": [{
                "id": "t1",
                "name": "log",
                "function": { "name": "log", "input": { "message": "x" } }
            }]
        }),
        json!({}), // task_details not set
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}", channel_name),
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = app
        .oneshot(json_request("GET", "/api/v1/data/traces", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let row = body["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == channel_name))
        .expect("expected a trace row");
    // task_trace_json should be absent (Option::None serialized as missing).
    assert!(
        row.get("task_trace_json").is_none() || row["task_trace_json"].is_null(),
        "task_trace_json should be omitted when task_details is unset, row={row:?}"
    );
}
