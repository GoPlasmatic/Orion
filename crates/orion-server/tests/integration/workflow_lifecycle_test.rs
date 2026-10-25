//! Workflow definitions run once through `POST /definitions/test`, refused
//! at upload, and split across revisions by a channel member's `rollouts`.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

// ============================================================
// 1. Dry-run with matching condition and computed output
// ============================================================

#[tokio::test]
async fn test_dry_run_with_matching_condition() {
    let app = common::test_app().await;

    // A workflow with parse_json -> map. The test endpoint puts the request
    // body's "data" field into the message payload. parse_json copies the
    // payload into data.input, then map computes data.computed from it.
    let workflow = json!({
        "name": "Dry Run Match",
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
                "name": "Compute result",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [{
                            "path": "data.computed",
                            "logic": {"*": [{"var": "data.input.x"}, 2]}
                        }]
                    }
                }
            }
        ]
    });

    // Dry-run the workflow with input data
    let (status, body) = common::deploy::test_run(&app, &workflow, json!({"x": 21}), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Should match (condition is true)
    assert_eq!(body["matched"], true);

    // Output should contain the computed value
    assert_eq!(body["output"]["computed"], 42);

    // Trace should be present and non-empty
    assert!(body["trace"].is_object());
    let steps = body["trace"]["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "trace should contain at least one step");

    // No errors
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.is_empty(), "should have no errors");
}

// ============================================================
// 2. Dry-run with unmatched condition
// ============================================================

#[tokio::test]
async fn test_dry_run_unmatched_condition() {
    let app = common::test_app().await;

    // The condition is at the workflow level, so it is evaluated BEFORE any
    // tasks run. The test endpoint runs from the message a node builds, whose
    // data is seeded with the request's: a condition like
    // {">": [{"var": "data.priority"}, 5]} reads the caller's `priority`.
    //
    // We test the unmet case: the workflow condition is false, so only a
    // workflow-level skip step is recorded and no tasks execute.
    let workflow = json!({
        "name": "Dry Run Unmatched",
        "condition": {">": [{"var": "data.priority"}, 5]},
        "tasks": [
            {
                "id": "t1",
                "name": "Log",
                "function": {
                    "name": "log",
                    "input": {"message": "should not run"}
                }
            }
        ]
    });

    // Dry-run with priority = 1. The run's data is seeded with its payload,
    // so the condition reads `data.priority` as 1 and `1 > 5` is false.
    let (status, body) =
        common::deploy::test_run(&app, &workflow, json!({"priority": 1}), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The trace should contain a workflow-level skip step
    let steps = body["trace"]["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "trace should have at least one step");

    // All steps should be "skipped" — no task was actually executed
    let all_skipped = steps.iter().all(|s| s["result"] == "skipped");
    assert!(
        all_skipped,
        "all steps should be skipped when condition is unmet"
    );

    // No task-level steps should be present (task_id should be null for workflow skip)
    let has_task_execution = steps.iter().any(|s| s["task_id"].is_string());
    assert!(
        !has_task_execution,
        "no tasks should have executed when workflow condition is false"
    );

    // Output is the input as the run started from it: a run's data is
    // seeded with its payload, and no task wrote to it.
    assert_eq!(
        body["output"],
        json!({"priority": 1}),
        "nothing is written when the workflow is skipped"
    );
}

// ============================================================
// 3. Dry-run with connector functions (cache_write)
// ============================================================

#[tokio::test]
async fn test_dry_run_with_connector_functions() {
    let app = common::test_app().await;

    // Deploy a memory cache connection
    let connector_id =
        common::create_connector(&app, common::cache_connector_memory("dry-run-cache")).await;
    assert!(!connector_id.is_empty());

    // A workflow that writes to the cache
    let workflow = json!({
        "name": "Dry Run Cache Write",
        "condition": true,
        "tasks": [
            {
                "id": "t1",
                "name": "Write to cache",
                "function": {
                    "name": "cache_write",
                    "input": {
                        "connector": "dry-run-cache",
                        "key": "test",
                        "value": "dry-run-value"
                    }
                }
            }
        ]
    });

    // Dry-run the workflow — cache_write should execute successfully
    let (status, body) = common::deploy::test_run(&app, &workflow, json!({}), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["matched"], true);

    // No errors from the cache_write execution
    let errors = body["errors"].as_array().unwrap();
    assert!(
        errors.is_empty(),
        "cache_write should not produce errors: {errors:?}"
    );
}

// ============================================================
// 4. Upload refuses a workflow that would not serve
// ============================================================

#[tokio::test]
async fn test_upload_validation() {
    let app = common::test_app().await;
    let upload = |workflow: Value| {
        let app = app.clone();
        async move {
            common::deploy::try_upload(&app, common::deploy::workflow_definition(&workflow)).await
        }
    };

    // --- Valid workflow ---
    let (status, body) = upload(json!({
        "name": "Valid Workflow",
        "condition": true,
        "tasks": [{"id":"t1","name":"Log","function":{"name":"log","input":{"message":"ok"}}}]
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // --- Empty tasks ---
    let (status, body) = upload(json!({
        "name": "No Tasks",
        "condition": true,
        "tasks": []
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let paths = common::deploy::refused_paths(&body);
    assert!(
        paths.iter().any(|p| p == "definition.tasks"),
        "should have error on 'tasks' field: {body}"
    );

    // --- Duplicate task IDs ---
    let (status, body) = upload(json!({
        "name": "Duplicate IDs",
        "condition": true,
        "tasks": [
            {"id":"dup","name":"First","function":{"name":"log","input":{"message":"a"}}},
            {"id":"dup","name":"Second","function":{"name":"log","input":{"message":"b"}}}
        ]
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let has_dup_error = body["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|e| {
            e["message"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("duplicate")
        });
    assert!(has_dup_error, "should report duplicate task IDs: {body}");
}

// ============================================================
// 5. Rollout partitions
// ============================================================

/// Upload `marker`'s workflow — one task writing `data.version` — answering
/// its digest.
async fn upload_marker_workflow(app: &axum::Router, marker: &str) -> String {
    common::deploy::upload(
        app,
        common::deploy::workflow_definition(&json!({
            "name": "Split",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Mark the version",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.version", "logic": marker}]}
                }
            }]
        })),
    )
    .await
}

/// Upload a sync HTTP channel named `name` on `/{name}`, answering its
/// digest.
async fn upload_channel(app: &axum::Router, name: &str) -> String {
    common::deploy::upload(
        app,
        json!({"kind": "channel", "definition": {
            "name": name,
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": format!("/{name}"),
        }}),
    )
    .await
}

/// A manifest member.
fn member(kind: &str, id: &str, revision: u64, digest: &str) -> Value {
    json!({
        "entity": {"kind": kind, "id": id, "revision": revision},
        "definition_digest": digest,
        "requires": [],
    })
}

/// A channel member bound to `workflow`'s `revisions`, split by `rollouts`
/// (`(workflow_revision, start_bucket, end_bucket)`).
fn channel_member(
    id: &str,
    digest: &str,
    workflow: &str,
    revisions: &[u64],
    rollouts: &[(u64, u64, u64)],
) -> Value {
    let mut m = member("channel", id, 1, digest);
    m["requires"] = revisions
        .iter()
        .map(|r| json!({"kind": "workflow", "id": workflow, "revision": r}))
        .collect();
    m["rollouts"] = rollouts
        .iter()
        .map(|(rev, start, end)| {
            json!({"workflow_revision": rev, "start_bucket": start, "end_bucket": end})
        })
        .collect();
    m
}

/// A channel's rollouts must partition every bucket, each interval naming a
/// workflow revision once: a gap, an empty interval, or a bound past 100 is
/// refused by the apply, and nothing is deployed. A single revision over the
/// whole range is accepted, and the deployment keeps the split as written.
#[tokio::test]
async fn test_rollout_traffic_split() {
    let app = common::test_app().await;
    let workflow = upload_marker_workflow(&app, "v1").await;
    let channel = upload_channel(&app, "rollout-test").await;

    for (rollouts, why) in [
        (
            vec![(1, 0, 50)],
            "a partial rollout of one revision leaves a gap",
        ),
        (vec![(1, 0, 0)], "an empty interval is refused"),
        (vec![(1, 0, 101)], "a bound past 100 is refused"),
    ] {
        let (base, _) = common::deploy::head(&app).await;
        let (status, body) = common::deploy::apply(
            &app,
            base,
            vec![
                member("workflow", "rollout-test", 1, &workflow),
                channel_member("rollout-test", &channel, "rollout-test", &[1], &rollouts),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(
            body["error"]["code"], "ROLLOUT_PARTITION_INVALID",
            "{why}: {body}"
        );
    }

    let (base, _) = common::deploy::head(&app).await;
    let (status, body) = common::deploy::apply(
        &app,
        base,
        vec![
            member("workflow", "rollout-test", 1, &workflow),
            channel_member(
                "rollout-test",
                &channel,
                "rollout-test",
                &[1],
                &[(1, 0, 100)],
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");

    // The split is what the deployment holds.
    let (_, members) = common::deploy::head(&app).await;
    let channel = members
        .iter()
        .find(|m| m["entity"]["kind"] == "channel")
        .expect("the channel member");
    assert_eq!(
        channel["rollouts"],
        json!([{"workflow_revision": 1, "start_bucket": 0, "end_bucket": 100}]),
        "{channel}"
    );
}

// ============================================================
// Rollout actually routes traffic (not just stores percentages)
// ============================================================

/// A 50/50 split must reach both revisions.
///
/// This is the end-to-end assertion the rollout mechanism never had. The
/// traffic split was once wired by wrapping each version's condition with
/// `{">=": [{"var": "_rollout_bucket"}, min]}` — a **context-root** lookup —
/// while the ingress injected the key at `data._rollout_bucket`. The lookup
/// therefore resolved to null, coerced to `0`, and only the version whose
/// range started at 0 ever matched: 100% of traffic to one version, whatever
/// the configured percentages said. The only test covering it asserted on the
/// *shape* of the generated condition and never evaluated one.
///
/// dataflow-rs routes on `Workflow::rollout` against
/// `Message::routing_bucket`, so there is no expression and no namespace to
/// misspell. Here two revisions of one workflow are deployed beside a
/// channel whose `rollouts` give each half the buckets. Buckets are random
/// per request when the caller has no sticky identity, so this drives
/// enough requests that a one-sided split is not a plausible outcome: at
/// 50/50 over 60 requests, missing a revision has probability 2⁻⁵⁹.
#[tokio::test]
async fn rollout_percentages_split_traffic_across_versions() {
    let app = common::test_app().await;

    let v1 = upload_marker_workflow(&app, "v1").await;
    let v2 = upload_marker_workflow(&app, "v2").await;
    let channel = upload_channel(&app, "split").await;

    let (base, _) = common::deploy::head(&app).await;
    let (status, body) = common::deploy::apply(
        &app,
        base,
        vec![
            member("workflow", "split", 1, &v1),
            member("workflow", "split", 2, &v2),
            channel_member(
                "split",
                &channel,
                "split",
                &[1, 2],
                &[(1, 0, 50), (2, 50, 100)],
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");

    let mut seen = std::collections::HashSet::new();
    for _ in 0..60 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/data/split",
                Some(json!({"data": {}})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let version = body["data"]["version"]
            .as_str()
            .unwrap_or_else(|| panic!("every request must match a revision, got: {body}"))
            .to_string();
        seen.insert(version);
    }

    let mut seen: Vec<String> = seen.into_iter().collect();
    seen.sort();
    assert_eq!(
        seen,
        vec!["v1".to_string(), "v2".to_string()],
        "a 50/50 rollout must reach both revisions"
    );
}

/// A dry run that dies partway still reports the steps that ran.
///
/// `process_message_with_trace` once built the trace as a function-local and
/// moved it into the `Ok` arm, so a hard failure discarded every step already
/// recorded — on the one endpoint whose entire purpose is showing them.
/// dataflow-rs 3.1 fixed that with `process_message_tracing`.
///
/// Note the failing task's own step is still not recorded: the engine
/// propagates before appending it, so the trace ends at the last known-good
/// step and `error` names what stopped it.
#[tokio::test]
async fn a_failing_dry_run_returns_the_steps_that_ran() {
    let app = common::test_app().await;

    let workflow = json!({
        "name": "Partial Dry Run",
        "condition": true,
        "tasks": [
            {
                "id": "ok",
                "name": "This one succeeds",
                "function": {
                    "name": "map",
                    "input": {"mappings": [{"path": "data.reached", "logic": true}]}
                }
            },
            {
                "id": "boom",
                "name": "This one cannot resolve its connector",
                "function": {
                    "name": "http_call",
                    "input": {"connector": "no-such-connector", "path": "/x"}
                }
            }
        ]
    });

    let (status, body) = common::deploy::test_run(&app, &workflow, json!({}), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a failed dry run must report its trace, not a bare 5xx: {body}"
    );

    assert!(
        body["error"].is_string(),
        "the failure must be named, got: {body}"
    );

    let steps = body["trace"]["steps"].as_array().unwrap();
    assert!(
        steps.iter().any(|s| s["task_id"] == "ok"),
        "the step that ran before the failure must survive it, got: {body}"
    );
    assert_eq!(
        body["output"]["reached"], true,
        "and its writes must be visible in the output, got: {body}"
    );
}
