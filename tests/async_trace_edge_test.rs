mod common;

use axum::http::StatusCode;
use common::{body_json, json_request, poll_trace_until_done};
use serde_json::json;
use tower::ServiceExt;

// ============================================================
// Async trace with a workflow that produces a result
// ============================================================

#[tokio::test]
async fn test_async_trace_result_persisted() {
    let app = common::test_app().await;

    // Create a workflow with a map task that sets a result field
    common::create_and_activate_channel(
        &app,
        "persist-async",
        json!({
            "name": "Result Workflow",
            "condition": true,
            "tasks": [{
                "id": "map1",
                "name": "Set result",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.computed",
                            "logic": { "cat": ["hello-", { "var": "data.input" }] }
                        }]
                    }
                }
            }]
        }),
    )
    .await;

    // Submit async
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/persist-async/async",
            Some(json!({"data": {"input": "world"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap();

    // Poll until completed
    let trace = poll_trace_until_done(&app, trace_id, 40).await;
    assert_eq!(trace["status"], "completed");

    // The trace message context should contain the mapped data
    let computed = &trace["message"]["context"]["data"]["computed"];
    assert!(
        computed.is_string(),
        "completed async trace should contain mapped result data"
    );
}

// ============================================================
// Async trace with no matching workflow completes (no-op)
// ============================================================

#[tokio::test]
async fn test_async_trace_no_matching_workflow_completes() {
    let app = common::test_app().await;

    // Submit to a non-existent channel — should still get 202
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/ghost-channel/async",
            Some(json!({"data": {"key": "value"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap();

    // Trace should complete (engine processes it as no-op)
    let trace = poll_trace_until_done(&app, trace_id, 40).await;
    assert_eq!(trace["status"], "completed");
}

// ============================================================
// Multiple async traces from same channel complete independently
// ============================================================

#[tokio::test]
async fn test_async_traces_complete_independently() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "multi-async",
        common::simple_log_workflow("Multi Async WF"),
    )
    .await;

    // Submit two async traces
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/multi-async/async",
            Some(json!({"data": {"batch": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let trace_id_1 = body_json(resp).await["trace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/multi-async/async",
            Some(json!({"data": {"batch": 2}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let trace_id_2 = body_json(resp).await["trace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Both should be different trace IDs
    assert_ne!(trace_id_1, trace_id_2);

    // Both should complete
    let trace1 = poll_trace_until_done(&app, &trace_id_1, 40).await;
    let trace2 = poll_trace_until_done(&app, &trace_id_2, 40).await;
    assert_eq!(trace1["status"], "completed");
    assert_eq!(trace2["status"], "completed");
}
