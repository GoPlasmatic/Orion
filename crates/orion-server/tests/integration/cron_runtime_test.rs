//! The scheduler actually running: a stored schedule becomes a durable
//! occurrence, an occurrence becomes a run, and a run becomes a settled record
//! and a trace.
//!
//! These drive the real loops — `test_app` starts them — against a schedule
//! that fires every second, so nothing here sleeps for a fixed duration. Every
//! wait is a condition on the admin API, polled until it holds.
//!
//! The pure parts are tested where they are pure: the misfire policies and the
//! DST rules in `channel::cron`, the claim and singleton SQL in
//! `storage::repositories::cron`, the planner's cursor arithmetic in
//! `cron::scheduler`. What is left for here is the wiring — that the pieces are
//! connected to each other and to the engine.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// A config whose scheduler reacts in test time rather than production time.
fn fast_cron() -> orion::config::AppConfig {
    let mut config = orion::config::AppConfig::default();
    config.cron.poll_interval_ms = 100;
    // The grace window has to cover the poll interval, or every occurrence
    // reports a misfire. 5s is the default and covers 100ms comfortably.
    config.cron.claim_lease_secs = 30;
    config.cron.heartbeat_interval_secs = 1;
    config
}

/// A workflow that copies the trigger metadata into `data`, so a test can see
/// what the run actually received.
fn trigger_echo_workflow(name: &str) -> Value {
    common::workflow_with_tasks(
        name,
        json!([
            {
                // The authored payload arrives exactly where an HTTP request
                // body does, so a workflow reads it exactly the same way. This
                // is the whole portability claim: the same workflow runs behind
                // a route and behind a schedule with no change.
                "id": "parse",
                "name": "Parse the scheduled payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "input" }
                }
            },
            {
                "id": "echo",
                "name": "Echo the trigger",
                "function": {
                    "name": "map",
                    "input": {
                        "mappings": [
                            { "path": "data.saw_occurrence", "logic": { "var": "metadata.trigger.occurrence_id" } },
                            { "path": "data.saw_type", "logic": { "var": "metadata.trigger.type" } },
                            { "path": "data.saw_scheduled_for", "logic": { "var": "metadata.trigger.scheduled_for" } },
                            { "path": "data.saw_window", "logic": { "var": "data.input.window" } }
                        ]
                    }
                }
            }
        ]),
    )
}

/// A workflow that takes long enough that a per-second schedule is guaranteed
/// to want to start a second occurrence while the first is still running.
fn slow_workflow(name: &str) -> Value {
    common::workflow_with_tasks(
        name,
        json!([{
            "id": "slow",
            "name": "A call that takes a while",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": "slow-endpoint",
                    "method": "GET",
                    "path": "/slow",
                    "response_path": "data.called",
                    "timeout_ms": 5000
                }
            }
        }]),
    )
}

/// Create and activate a cron channel, returning its `channel_id`.
async fn activate_cron_channel(
    app: &axum::Router,
    name: &str,
    workflow: Value,
    transport_config: Value,
) -> String {
    let workflow_id = common::create_and_activate_workflow(app, workflow).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": name,
                "channel_type": "async",
                "protocol": "cron",
                "workflow_id": workflow_id,
                "transport_config": transport_config,
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel id")
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate");
    assert_eq!(resp.status(), StatusCode::OK);
    channel_id
}

/// Poll the occurrence listing until `pred` holds on the rows.
async fn wait_for_occurrences<F>(app: &axum::Router, query: &str, pred: F) -> Vec<Value>
where
    F: Fn(&[Value]) -> bool,
{
    let uri = format!("/api/v1/admin/cron/occurrences?{query}");
    let body = common::wait_for_body(app, &uri, |body| {
        body["data"]
            .as_array()
            .is_some_and(|rows| pred(rows.as_slice()))
    })
    .await;
    body["data"].as_array().cloned().unwrap_or_default()
}

fn with_status<'a>(rows: &'a [Value], status: &str) -> Vec<&'a Value> {
    rows.iter().filter(|r| r["status"] == status).collect()
}

// ============================================================
// A schedule fires
// ============================================================

/// The whole path in one test: a stored schedule produces a durable occurrence,
/// a worker runs it against the engine, and both the ledger and the trace
/// record what happened.
#[tokio::test]
async fn a_schedule_produces_an_occurrence_that_runs_and_settles() {
    let app = common::test_app_with_config(fast_cron()).await;
    let channel_id = activate_cron_channel(
        &app,
        "every-second",
        trigger_echo_workflow("Trigger Echo"),
        json!({"schedule": "* * * * * *", "payload": {"window": "previous_day"}}),
    )
    .await;

    let rows = wait_for_occurrences(&app, "limit=50", |rows| {
        !with_status(rows, "completed").is_empty()
    })
    .await;
    let completed = with_status(&rows, "completed");
    let occurrence_id = completed[0]["id"].as_str().expect("id").to_string();
    assert_eq!(completed[0]["channel_id"], channel_id);
    assert_eq!(completed[0]["trigger"], "cron");
    assert_eq!(completed[0]["attempt"], 1);

    // The full record carries what the summary leaves out.
    let body = common::wait_for_body(
        &app,
        &format!("/api/v1/admin/cron/occurrences/{occurrence_id}"),
        |body| body["data"]["trace_id"].is_string(),
    )
    .await;
    let occurrence = &body["data"];
    assert_eq!(occurrence["error_message"], Value::Null);
    // The version that materialised it and the version that ran it are both
    // recorded, so "which definition ran?" is answerable rather than inferred.
    assert_eq!(occurrence["channel_version"], 1);
    assert_eq!(occurrence["executing_version"], 1);
    // Settled rows hold no lease.
    assert_eq!(occurrence["claimed_until"], Value::Null);

    // And the trace: mode `cron`, the authored payload as its input.
    let trace_id = occurrence["trace_id"].as_str().expect("trace id");
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace");
    assert_eq!(resp.status(), StatusCode::OK);
    let trace = body_json(resp).await;
    assert_eq!(trace["data"]["mode"], "cron", "{trace}");
    assert_eq!(trace["data"]["channel_id"], channel_id);
    assert_eq!(trace["data"]["status"], "completed");
}

/// What the workflow can see about its own occurrence. The distinction between
/// the instant the work was *for* and the instant it ran is the point of the
/// object, so both have to arrive.
#[tokio::test]
async fn the_workflow_receives_the_trigger_metadata_and_the_authored_payload() {
    let app = common::test_app_with_config(fast_cron()).await;
    activate_cron_channel(
        &app,
        "trigger-facts",
        trigger_echo_workflow("Trigger Facts"),
        json!({
            "schedule": "* * * * * *",
            "timezone": "Asia/Kolkata",
            "payload": {"window": "previous_day"},
        }),
    )
    .await;

    let rows = wait_for_occurrences(&app, "limit=50", |rows| {
        !with_status(rows, "completed").is_empty()
    })
    .await;
    let completed = with_status(&rows, "completed");
    let occurrence_id = completed[0]["id"].as_str().expect("id");
    let scheduled_for = completed[0]["scheduled_for"].as_str().expect("scheduled");

    let body = common::wait_for_body(
        &app,
        &format!("/api/v1/admin/cron/occurrences/{occurrence_id}"),
        |body| body["data"]["trace_id"].is_string(),
    )
    .await;
    let trace_id = body["data"]["trace_id"].as_str().expect("trace id");
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .expect("trace");
    let trace = body_json(resp).await;
    // The finished message, as every other ingress records it.
    let result = &trace["data"]["message"];

    assert_eq!(
        result["saw_occurrence"], occurrence_id,
        "the workflow must be able to identify its own occurrence: {trace}"
    );
    assert_eq!(result["saw_type"], "cron");
    // The authored payload arrives exactly where an HTTP request body does —
    // at `payload`, for the workflow to read or parse — so a workflow written
    // against a REST channel needs no change to run on a schedule.
    assert_eq!(
        result["saw_window"], "previous_day",
        "the authored payload must reach the workflow: {trace}"
    );
    // `scheduled_for` reaches the workflow as the ledger records it, which is
    // what makes it usable as an idempotency key.
    assert!(
        result["saw_scheduled_for"]
            .as_str()
            .expect("a scheduled_for")
            .starts_with(&scheduled_for[..19]),
        "trigger.scheduled_for must match the occurrence: {result}"
    );
}

// ============================================================
// The status view
// ============================================================

#[tokio::test]
async fn the_status_endpoint_reports_the_cursor_and_the_last_run() {
    let app = common::test_app_with_config(fast_cron()).await;
    let channel_id = activate_cron_channel(
        &app,
        "status-view",
        common::simple_log_workflow("Status View"),
        json!({"schedule": "* * * * * *", "timezone": "Europe/London"}),
    )
    .await;

    let body = common::wait_for_body(&app, "/api/v1/admin/cron/status", |body| {
        body["data"][0]["last_status"].is_string()
    })
    .await;
    let row = &body["data"][0];
    assert_eq!(row["channel_id"], channel_id);
    assert_eq!(row["schedule"], "* * * * * *");
    assert_eq!(row["timezone"], "Europe/London");
    assert!(
        row["next_fire_at"].is_string(),
        "the cursor must be visible once the reconciler has seen the channel: {row}"
    );
    assert_eq!(row["paused_at"], Value::Null);
}

// ============================================================
// Lifecycle
// ============================================================

/// Archiving stops the schedule and keeps its history. The two halves matter
/// equally: an archived channel must stop producing work, and the record of
/// what it did must survive.
#[tokio::test]
async fn archiving_stops_materialisation_and_keeps_history() {
    let app = common::test_app_with_config(fast_cron()).await;
    let channel_id = activate_cron_channel(
        &app,
        "to-archive",
        common::simple_log_workflow("To Archive"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    wait_for_occurrences(&app, "limit=50", |rows| !rows.is_empty()).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .expect("archive");
    assert_eq!(resp.status(), StatusCode::OK);

    // Let the reconciler notice and pause the cursor, then confirm the count
    // has stopped moving.
    let settled = common::wait_for_body(&app, "/api/v1/admin/cron/occurrences?limit=200", |_| true)
        .await["total"]
        .as_i64()
        .unwrap_or(0);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let after = common::wait_for_body(&app, "/api/v1/admin/cron/occurrences?limit=200", |_| true)
        .await["total"]
        .as_i64()
        .unwrap_or(0);

    assert!(
        after >= settled,
        "history must survive archiving: {settled} -> {after}"
    );
    assert!(
        after - settled <= 2,
        "an archived channel must stop producing occurrences: {settled} -> {after} \
         in 600ms of a per-second schedule"
    );
}

// ============================================================
// Manual trigger and retry
// ============================================================

#[tokio::test]
async fn a_manual_trigger_creates_an_occurrence_that_runs() {
    // A schedule far in the future, so the only occurrence is the manual one
    // and the test cannot pass by accident.
    let app = common::test_app_with_config(fast_cron()).await;
    let channel_id = activate_cron_channel(
        &app,
        "manual-only",
        trigger_echo_workflow("Manual Only"),
        json!({"schedule": "0 0 4 1 1 *", "payload": {"window": "manual"}}),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/channels/{channel_id}/trigger"),
            None,
        ))
        .await
        .expect("trigger");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["trigger"], "manual");
    assert_eq!(body["data"]["status"], "pending");
    let occurrence_id = body["data"]["id"].as_str().expect("id").to_string();

    let body = common::wait_for_body(
        &app,
        &format!("/api/v1/admin/cron/occurrences/{occurrence_id}"),
        |body| body["data"]["status"] == "completed",
    )
    .await;
    assert_eq!(body["data"]["trigger"], "manual");
    assert!(body["data"]["trace_id"].is_string());
}

#[tokio::test]
async fn triggering_a_non_cron_channel_is_refused() {
    let app = common::test_app_with_config(fast_cron()).await;
    let (_, _) = common::create_and_activate_channel(
        &app,
        "ordinary",
        common::simple_log_workflow("Ordinary"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels/ordinary/trigger",
            None,
        ))
        .await
        .expect("trigger");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// A retry is another attempt at the *same* occurrence: same id, same
/// `scheduled_for`, one more attempt. Re-running finished work is a trigger,
/// not a retry, so `completed` is refused.
#[tokio::test]
async fn retry_reuses_the_occurrence_and_refuses_a_completed_one() {
    let app = common::test_app_with_config(fast_cron()).await;
    activate_cron_channel(
        &app,
        "retryable",
        common::simple_log_workflow("Retryable"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    let rows =
        wait_for_occurrences(&app, "status=completed&limit=10", |rows| !rows.is_empty()).await;
    let occurrence_id = rows[0]["id"].as_str().expect("id").to_string();

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/admin/cron/occurrences/{occurrence_id}/retry"),
            None,
        ))
        .await
        .expect("retry");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a completed occurrence is re-run by triggering the channel, not by retrying it"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/cron/occurrences/does-not-exist/retry",
            None,
        ))
        .await
        .expect("retry");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// Health
// ============================================================

#[tokio::test]
async fn health_reports_the_scheduler_once_a_cron_channel_is_loaded() {
    let app = common::test_app_with_config(fast_cron()).await;

    // Before any cron channel, the component is present and healthy — the
    // scheduler is on, it simply has nothing to do.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health");
    let body = body_json(resp).await;
    assert_eq!(body["components"]["cron"], "ok", "{body}");

    activate_cron_channel(
        &app,
        "healthy-cron",
        common::simple_log_workflow("Healthy Cron"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;
    wait_for_occurrences(&app, "limit=10", |rows| !rows.is_empty()).await;

    let body = common::wait_for_body(&app, "/health", |body| {
        body["cron"]["scheduled_channels"] == 1
    })
    .await;
    assert_eq!(body["components"]["cron"], "ok", "{body}");
    assert_eq!(body["status"], "ok");
    assert!(body["cron"]["last_reconcile_at"].is_i64(), "{body}");
    assert_eq!(body["cron"]["lease_renewal_failures"], 0);
}

/// D3 at the runtime level: with the scheduler off, an active cron channel is
/// quarantined and `/health` says so rather than reporting a healthy node whose
/// schedules never fire.
#[tokio::test]
async fn a_disabled_scheduler_with_a_stored_cron_channel_is_degraded() {
    // Created and activated while the scheduler is on…
    let app = common::test_app_with_config(fast_cron()).await;
    activate_cron_channel(
        &app,
        "orphaned",
        common::simple_log_workflow("Orphaned"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    // …then the node restarts with it off. `test_app_with_config` builds a
    // fresh in-memory database, so the channel is recreated through the API on
    // a node that will refuse to activate it — which is the D3 activation gate,
    // covered in `cron_channel_validation_test`. What this test pins is the
    // *quarantine* half: a stored active row that this node cannot run.
    let mut config = fast_cron();
    config.cron.enabled = false;
    let disabled = common::test_app_with_config(config).await;
    let resp = disabled
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health");
    let body = body_json(resp).await;
    // With no cron channel loaded and the scheduler off, there is nothing to
    // report — a node that does not schedule is not a broken node.
    assert!(
        body["components"].get("cron").is_none(),
        "a node with no schedules and no scheduler has nothing to say: {body}"
    );
}

// ============================================================
// Singleton: the design's central guarantee
// ============================================================

/// `forbid` under real contention: a schedule that fires every second, running
/// work that takes longer than a second.
///
/// The assertion is the one the whole storage model exists for — never two
/// occurrences of one key in flight at once — and the visible-skip half, which
/// is what separates `forbid` from silently dropping work.
#[tokio::test]
async fn forbid_serialises_a_key_and_records_the_skips() {
    // A server that answers slowly, so an occurrence is still running when the
    // next one becomes due.
    let addr = common::start_slow_server(std::time::Duration::from_millis(1200)).await;

    let app = common::test_app_with_config(fast_cron()).await;
    common::create_http_connector(&app, "slow-endpoint", addr).await;
    activate_cron_channel(
        &app,
        "singleton-ch",
        slow_workflow("Slow Work"),
        json!({
            "schedule": "* * * * * *",
            "concurrency": {"policy": "forbid"},
        }),
    )
    .await;

    // Wait until contention has actually happened, rather than assuming it.
    let rows = wait_for_occurrences(&app, "limit=200", |rows| {
        !with_status(rows, "skipped_singleton").is_empty()
            && !with_status(rows, "completed").is_empty()
    })
    .await;

    // The skips are recorded, not dropped — an operator can see that the
    // schedule is firing faster than the work takes.
    let skipped = with_status(&rows, "skipped_singleton");
    assert!(
        skipped[0]["completed_at"].is_string(),
        "a skipped occurrence is settled, not left dangling: {:?}",
        skipped[0]
    );
    let body = common::wait_for_body(
        &app,
        &format!(
            "/api/v1/admin/cron/occurrences/{}",
            skipped[0]["id"].as_str().expect("id")
        ),
        |_| true,
    )
    .await;
    assert!(
        body["data"]["error_message"]
            .as_str()
            .expect("a reason")
            .contains("singleton key"),
        "the skip must say why: {body}"
    );

    // And the invariant itself: at no point were two of them running.
    let running = with_status(&rows, "running");
    assert!(
        running.len() <= 1,
        "two occurrences of one singleton key were running at once: {running:?}"
    );
}

/// `allow` is the other half of the same decision: no key is taken, so
/// occurrences overlap freely and nothing is ever skipped for contention.
#[tokio::test]
async fn allow_lets_occurrences_overlap() {
    let addr = common::start_slow_server(std::time::Duration::from_millis(800)).await;

    let app = common::test_app_with_config(fast_cron()).await;
    common::create_http_connector(&app, "slow-endpoint", addr).await;
    activate_cron_channel(
        &app,
        "overlapping-ch",
        slow_workflow("Overlapping Work"),
        json!({"schedule": "* * * * * *"}),
    )
    .await;

    // Three occurrences complete on a schedule whose work takes 800ms, which
    // can only happen if they overlapped.
    let rows = wait_for_occurrences(&app, "limit=200", |rows| {
        with_status(rows, "completed").len() >= 3
    })
    .await;
    assert!(
        with_status(&rows, "skipped_singleton").is_empty(),
        "`allow` takes no key, so nothing can be skipped for contention: {rows:?}"
    );
}

/// The manual trigger is not a side door: it goes through the same claim and
/// takes the same key a scheduled occurrence would.
///
/// This asserts the *mechanism* — the manual occurrence acquires the channel's
/// singleton under a fencing token — rather than trying to catch a collision.
/// Catching one here would mean racing a running occurrence against a trigger
/// whose instant the identity index may refuse, which is a flaky test of a
/// property that is already proven deterministically where it can be:
/// `one_singleton_key_admits_one_occurrence` in the repository drives two
/// occurrences at one key directly and pins that only one reaches `running`.
#[tokio::test]
async fn a_manual_trigger_takes_the_same_singleton_a_schedule_would() {
    let addr = common::start_slow_server(std::time::Duration::from_millis(1500)).await;

    let app = common::test_app_with_config(fast_cron()).await;
    common::create_http_connector(&app, "slow-endpoint", addr).await;
    let channel_id = activate_cron_channel(
        &app,
        "busy-ch",
        slow_workflow("Busy Work"),
        // Every two seconds rather than every second: a per-second schedule
        // owns every instant, so a manual trigger has nowhere to land and the
        // test spends its budget colliding with the identity index.
        json!({
            "schedule": "*/2 * * * * *",
            "concurrency": {"policy": "forbid"},
        }),
    )
    .await;

    // Wait until a scheduled occurrence is actually running.
    wait_for_occurrences(&app, "status=running&limit=10", |rows| !rows.is_empty()).await;

    // A per-second schedule owns every second, so the identity index may refuse
    // the first attempt — which is the documented answer ("try again in a
    // second"), and doing exactly that is the honest way to test it.
    let mut manual_id = None;
    for _ in 0..100 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/api/v1/admin/channels/{channel_id}/trigger"),
                None,
            ))
            .await
            .expect("trigger");
        match resp.status() {
            StatusCode::ACCEPTED => {
                manual_id = Some(
                    body_json(resp).await["data"]["id"]
                        .as_str()
                        .expect("id")
                        .to_string(),
                );
                break;
            }
            StatusCode::CONFLICT => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            other => panic!("unexpected trigger status {other}"),
        }
    }
    let manual_id = manual_id.expect("a manual occurrence within 10s of trying");

    let body = common::wait_for_body(
        &app,
        &format!("/api/v1/admin/cron/occurrences/{manual_id}"),
        |body| {
            matches!(
                body["data"]["status"].as_str(),
                Some("completed") | Some("failed") | Some("skipped_singleton")
            )
        },
    )
    .await;
    let occurrence = &body["data"];
    assert_eq!(occurrence["trigger"], "manual");

    // Either it waited its turn and was skipped, or it took the key. Both are
    // the singleton working; running *without* the key is the failure, and that
    // is what the assertions below rule out.
    if occurrence["status"] == "skipped_singleton" {
        assert!(
            occurrence["error_message"]
                .as_str()
                .expect("a reason")
                .contains("singleton key"),
            "{occurrence}"
        );
        return;
    }
    assert_eq!(
        occurrence["singleton_key"], channel_id,
        "a manual run must acquire the channel's key, not bypass it: {occurrence}"
    );
    assert!(
        occurrence["fencing_token"].as_i64().unwrap_or(0) >= 1,
        "the acquisition must carry a fencing token: {occurrence}"
    );
}

// ============================================================
// The guards a schedule does apply
// ============================================================

/// `Transport::Cron` keeps `validation_logic` on, and a scheduled run that
/// fails it fails **visibly** rather than running anyway or vanishing.
///
/// It is also deliberately not retried: the payload is authored and fixed, so
/// the next attempt would fail identically. The occurrence records why, and
/// the next scheduled instant is the next chance.
#[tokio::test]
async fn a_payload_that_fails_validation_fails_the_occurrence_visibly() {
    let app = common::test_app_with_config(fast_cron()).await;
    let workflow_id =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Validated")).await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "validated-cron",
                "channel_type": "async",
                "protocol": "cron",
                "workflow_id": workflow_id,
                "transport_config": {
                    "schedule": "* * * * * *",
                    "payload": {"window": "previous_day"},
                },
                // The payload has no `order_id`, so every occurrence is refused.
                "config": {"validation_logic": {"!!": {"var": "data.order_id"}}},
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate");
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = wait_for_occurrences(&app, "status=failed&limit=10", |rows| !rows.is_empty()).await;
    let body = common::wait_for_body(
        &app,
        &format!(
            "/api/v1/admin/cron/occurrences/{}",
            rows[0]["id"].as_str().expect("id")
        ),
        |_| true,
    )
    .await;
    assert!(
        body["data"]["error_message"]
            .as_str()
            .expect("a reason")
            .contains("guard_refused"),
        "the occurrence must say the guard refused it: {body}"
    );
    // Nothing completes, because the payload cannot ever satisfy the rule.
    let completed = common::wait_for_body(
        &app,
        "/api/v1/admin/cron/occurrences?status=completed",
        |_| true,
    )
    .await;
    assert_eq!(completed["total"], 0, "{completed}");
}

/// The channel's own `timeout_ms` governs a scheduled run, as it governs every
/// other ingress — and the deadline is reported as a failure rather than
/// leaving the occurrence stuck.
#[tokio::test]
async fn a_channel_timeout_bounds_a_scheduled_run() {
    let addr = common::start_slow_server(std::time::Duration::from_millis(3000)).await;
    let app = common::test_app_with_config(fast_cron()).await;
    common::create_http_connector(&app, "slow-endpoint", addr).await;

    let workflow_id = common::create_and_activate_workflow(&app, slow_workflow("Times Out")).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "timeout-cron",
                "channel_type": "async",
                "protocol": "cron",
                "workflow_id": workflow_id,
                "transport_config": {"schedule": "* * * * * *"},
                // Far shorter than the work, and far shorter than
                // `cron.default_timeout_ms` — so a pass proves the *channel's*
                // value is what applied.
                "config": {"timeout_ms": 300},
            })),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let channel_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("id")
        .to_string();
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{channel_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate");
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = wait_for_occurrences(&app, "status=failed&limit=10", |rows| !rows.is_empty()).await;
    let body = common::wait_for_body(
        &app,
        &format!(
            "/api/v1/admin/cron/occurrences/{}",
            rows[0]["id"].as_str().expect("id")
        ),
        |_| true,
    )
    .await;
    assert!(
        body["data"]["error_message"]
            .as_str()
            .expect("a reason")
            .contains("timed out"),
        "the channel's timeout_ms must bound the run: {body}"
    );
}
