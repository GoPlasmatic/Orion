//! A2 (Tier-1 ergonomics): per-task trace data (intermediate inputs/outputs).
//!
//! Verifies that:
//!   - the dry-run `/test` endpoint already returns per-step `message` snapshots
//!     from `dataflow_rs::ExecutionTrace` (no code change — locks in the behavior)
//!   - opting into `config.tracing.task_details = true` causes the engine to
//!     capture an `ExecutionTrace` and persist it as `task_trace_json` on the
//!     resulting trace row

use crate::common::{
    body_json, create_and_activate_channel_with_config, create_and_activate_workflow, json_request,
    poll_trace_until_done, test_app,
};
use axum::http::StatusCode;
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

    // Wait for the sync-mode trace persistence, then fetch the detail — the
    // list is a payload-free projection (S14), so task_trace_json is served
    // only by the single-trace GET.
    let body = crate::common::wait_for_body(&app, "/api/v1/data/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == channel_name))
    })
    .await;
    let row = body["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == channel_name))
        .expect("expected at least one trace row for the task-trace channel");
    assert!(
        row.get("task_trace_json").is_none() || row["task_trace_json"].is_null(),
        "list rows must not carry payloads (S14), row={row:?}"
    );
    let trace_id = row["id"].as_str().unwrap();

    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = body_json(resp).await;
    // The captured ExecutionTrace must be persisted and served on the detail.
    let parsed = &detail["task_trace_json"];
    let steps = parsed["steps"]
        .as_array()
        .expect("task_trace_json must be populated when task_details=true");
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

    let body = crate::common::wait_for_body(&app, "/api/v1/data/traces", |b| {
        b["data"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r["channel"] == channel_name))
    })
    .await;
    let row = body["data"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["channel"] == channel_name))
        .expect("expected a trace row");
    let trace_id = row["id"].as_str().unwrap();

    // task_trace_json should be absent on the detail (Option::None → missing).
    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    let detail = body_json(resp).await;
    assert!(
        detail.get("task_trace_json").is_none() || detail["task_trace_json"].is_null(),
        "task_trace_json should be omitted when task_details is unset, detail={detail:?}"
    );
}

#[tokio::test]
async fn sync_get_trace_endpoint_returns_task_trace_json() {
    // The single-trace GET handler builds its response field-by-field; this
    // guards against it dropping task_trace_json (it was write-only before).
    let app = test_app().await;

    let (channel_name, _) = create_and_activate_channel_with_config(
        &app,
        "get-trace-task-details",
        json!({
            "name": "get-trace-task-details-wf",
            "tasks": [{
                "id": "t1",
                "name": "log",
                "function": { "name": "log", "input": { "message": "ping" } }
            }]
        }),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}", channel_name),
            Some(json!({ "data": { "x": 1 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Find the trace row id, then fetch it via the single-trace GET endpoint.
    let body = crate::common::wait_for_body(
        &app,
        &format!("/api/v1/data/traces?channel={}", channel_name),
        |b| b["data"].as_array().is_some_and(|a| !a.is_empty()),
    )
    .await;
    let trace_id = body["data"][0]["id"]
        .as_str()
        .expect("expected a trace row")
        .to_string();

    let resp = app
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/data/traces/{}", trace_id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let steps = body["task_trace_json"]["steps"]
        .as_array()
        .expect("single-trace GET must return task_trace_json.steps when task_details=true");
    assert!(!steps.is_empty());
    assert!(steps.iter().any(|s| s["task_id"] == "t1"));
}

#[tokio::test]
async fn async_request_with_task_details_captures_and_returns_task_trace_json() {
    // Async traces are processed by the queue worker, which must use the
    // with-trace engine entrypoint and persist task_trace_json via set_result.
    let app = test_app().await;

    let (channel_name, _) = create_and_activate_channel_with_config(
        &app,
        "async-task-details",
        json!({
            "name": "async-task-details-wf",
            "tasks": [{
                "id": "t1",
                "name": "log",
                "function": { "name": "log", "input": { "message": "ping" } }
            }]
        }),
        json!({ "tracing": { "task_details": true } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{}/async", channel_name),
            Some(json!({ "data": { "x": 1 } })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    // poll_trace_until_done hits the single-trace GET endpoint.
    let body = poll_trace_until_done(&app, &trace_id, 40, Some(&token)).await;
    assert_eq!(
        body["status"], "completed",
        "trace did not complete: {body:?}"
    );
    let steps = body["task_trace_json"]["steps"]
        .as_array()
        .expect("async trace must capture + return task_trace_json.steps when task_details=true");
    assert!(!steps.is_empty());
    assert!(steps.iter().any(|s| s["task_id"] == "t1"));
}
