//! The cron channel as an *authored definition*: what definition upload
//! (`POST /api/v2/admin/definitions`) accepts, what it refuses, and the two
//! doors it closes.
//!
//! The refusals here are all one idea. A cron channel is started by a clock, so
//! every setting that describes a caller — who they are, where they came from,
//! how often they may ask, what they get back — is a setting Orion would accept
//! and never apply. Storing one would leave an operator reading an `auth` block
//! that protects nothing. So each is refused at the boundary, with the guard
//! matrix (`Transport::Cron`) and this validation kept in agreement by
//! `cron_guards_and_validation_agree` in the guards module.
//!
//! The execution half — occurrences, singletons, the ledger — is
//! `cron_scheduler_test` and `cron_worker_test`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, deploy, json_request};

/// A well-formed cron channel body. Tests override one field at a time.
fn cron_channel(name: &str, workflow_id: &str) -> Value {
    json!({
        "name": name,
        "channel_type": "async",
        "protocol": "cron",
        "workflow_id": workflow_id,
        "transport_config": {
            "schedule": "0 15 2 * * *",
            "timezone": "Asia/Kolkata",
            "payload": {"window": "previous_day"},
        },
        "config": {"timeout_ms": 60_000},
    })
}

/// Upload a v1-shaped channel body as a channel definition — its
/// `workflow_id` is the manifest's business, not the definition's — answering
/// the status and body as they are.
async fn post_channel(app: &axum::Router, mut body: Value) -> (StatusCode, Value) {
    body.as_object_mut()
        .expect("a channel")
        .remove("workflow_id");
    deploy::try_upload(app, json!({"kind": "channel", "definition": body})).await
}

/// The stored channel definition an accepted upload's `digest` names.
async fn read_back(app: &axum::Router, upload: &Value) -> Value {
    let digest = upload["digest"].as_str().expect("a digest");
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/definitions/{digest}"),
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["definition"].take()
}

/// Every `path` a refusal named, so a test can assert on the set rather than on
/// an index into it.
fn error_paths(body: &Value) -> Vec<String> {
    body["error"]["details"]
        .as_array()
        .map(|details| {
            details
                .iter()
                .filter_map(|d| d["path"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_well_formed_cron_channel_is_accepted_and_reads_back() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let (status, body) = post_channel(&app, cron_channel("nightly-rollup", &wf)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let body = read_back(&app, &body).await;
    assert_eq!(body["protocol"], "cron");
    // The schedule stays inside `transport_config`, which is what makes it
    // versioned and content-hashed with no new top-level field.
    assert_eq!(
        body["transport_config"]["schedule"], "0 15 2 * * *",
        "{body}"
    );
    assert_eq!(body["route_pattern"], Value::Null);
    assert_eq!(body["topic"], Value::Null);
}

/// The protocol is case-insensitive on the wire, like every other enum Orion
/// publishes, so a mixed-version client spelling it `"Cron"` still works.
#[tokio::test]
async fn the_protocol_is_case_insensitive() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("upper-cron", &wf);
    body["protocol"] = json!("CRON");
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(read_back(&app, &body).await["protocol"], "cron");
}

// ============================================================
// The schedule itself
// ============================================================

#[tokio::test]
async fn a_schedule_that_does_not_compile_is_refused_at_upload() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("broken", &wf);
    body["transport_config"]["schedule"] = json!("15 2 * * *"); // five fields
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        error_paths(&body),
        vec!["definition.transport_config.schedule"],
        "{body}"
    );
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .expect("message")
            .contains("exactly 6 fields"),
        "{body}"
    );
}

/// `slots` bounds a lock, so it means nothing without one, and its range is
/// what bounds the statements a skipped attempt costs. Both refusals name the
/// field; a valid bound is accepted and reads back.
#[tokio::test]
async fn concurrency_slots_are_bounded_and_forbid_only() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    for (name, concurrency, refusal) in [
        (
            "slots-allow",
            json!({"policy": "allow", "slots": 4}),
            "only meaningful with policy \"forbid\"",
        ),
        (
            "slots-zero",
            json!({"policy": "forbid", "slots": 0}),
            "between 1 and 64",
        ),
        (
            "slots-many",
            json!({"policy": "forbid", "slots": 65}),
            "between 1 and 64",
        ),
    ] {
        let mut body = cron_channel(name, &wf);
        body["transport_config"]["concurrency"] = concurrency;
        let (status, body) = post_channel(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
        assert_eq!(
            error_paths(&body),
            vec!["definition.transport_config.concurrency.slots"],
            "{name}: {body}"
        );
        assert!(
            body["error"]["details"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains(refusal)),
            "{name}: {body}"
        );
    }

    let mut body = cron_channel("slots-ok", &wf);
    body["transport_config"]["concurrency"] = json!({"policy": "forbid", "key": "w", "slots": 4});
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        read_back(&app, &body).await["transport_config"]["concurrency"]["slots"],
        4
    );
}

#[tokio::test]
async fn a_schedule_is_required() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("no-schedule", &wf);
    body["transport_config"] = json!({});
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// The typo class `deny_unknown_fields` exists for: a misspelled key is a
/// scheduling decision that would silently never have been applied.
#[tokio::test]
async fn a_misspelled_transport_key_names_itself() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("typo", &wf);
    body["transport_config"]["misfire_polcy"] = json!("skip");
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["details"][0]["code"], "UNKNOWN_FIELD",
        "{body}"
    );
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .expect("message")
            .contains("misfire_polcy"),
        "{body}"
    );
}

#[tokio::test]
async fn an_unknown_time_zone_is_refused() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("bad-zone", &wf);
    body["transport_config"]["timezone"] = json!("IST");
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        error_paths(&body),
        vec!["definition.transport_config.timezone"],
        "{body}"
    );
}

/// The payload is recorded verbatim in every occurrence's trace input, so a
/// secret placed there is a secret at rest in the traces table.
#[tokio::test]
async fn a_secret_in_the_payload_is_refused() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("leaky", &wf);
    body["transport_config"]["payload"] = json!({"token": {"secret": "stripe_key"}});
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["details"][0]["code"], "UNRESOLVED_SECRET_REF",
        "{body}"
    );
}

/// B1: an author with several problems learns about all of them in one round
/// trip.
#[tokio::test]
async fn every_problem_is_reported_at_once() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("many-problems", &wf);
    body["transport_config"] = json!({
        "schedule": "0 0 25 * * *",
        "timezone": "Mars/Olympus",
        "misfire_policy": "catch_up",
    });
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let paths = error_paths(&body);
    for expected in [
        "definition.transport_config.schedule",
        "definition.transport_config.timezone",
        "definition.transport_config.max_catch_up",
    ] {
        assert!(paths.contains(&expected.to_string()), "{expected}: {body}");
    }
}

// ============================================================
// Fields that belong to another protocol
// ============================================================

#[tokio::test]
async fn routing_fields_of_other_protocols_are_refused() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    for (field, value) in [
        ("methods", json!(["POST"])),
        ("route_pattern", json!("/nightly")),
    ] {
        let mut body = cron_channel(&format!("routed-{field}"), &wf);
        body[field] = value;
        let (status, body) = post_channel(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert!(
            error_paths(&body).contains(&format!("definition.{field}")),
            "{field}: {body}"
        );
    }

    // A channel definition has no Kafka fields at all, so these are refused
    // as unknown keys before channel validation runs.
    for (field, value) in [
        ("topic", json!("orders")),
        ("consumer_group", json!("group-a")),
    ] {
        let mut body = cron_channel(&format!("routed-{field}"), &wf);
        body.as_object_mut()
            .expect("a channel")
            .remove("workflow_id");
        body[field] = value;
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v2/admin/definitions",
                Some(json!({"kind": "channel", "definition": body})),
            ))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{field}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(&format!("unknown field `{field}`")),
            "{field}: {text}"
        );
    }
}

/// Nothing waits for a scheduled run, so `sync` would promise a caller a result
/// there is no caller to receive.
#[tokio::test]
async fn a_sync_cron_channel_is_refused() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("sync-cron", &wf);
    body["channel_type"] = json!("sync");
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        error_paths(&body).contains(&"definition.channel_type".to_string()),
        "{body}"
    );
}

// ============================================================
// Caller-shaped guards
// ============================================================

#[tokio::test]
async fn every_caller_shaped_guard_is_refused() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    for (key, value) in [
        ("auth", json!({"mode": "api_key", "keys": ["k"]})),
        ("origin_allow_list", json!(["https://example.com"])),
        ("rate_limit", json!({"requests_per_second": 10})),
        ("deduplication", json!({"header": "idempotency-key"})),
        ("cache", json!({"enabled": true, "ttl_secs": 60})),
        ("request", json!({"body_mode": "payload"})),
        ("response", json!({"mode": "shaped"})),
    ] {
        let mut body = cron_channel(&format!("guarded-{key}"), &wf);
        body["config"][key] = value;
        let (status, body) = post_channel(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {body}");
        assert!(
            error_paths(&body).contains(&format!("definition.config.{key}")),
            "{key}: {body}"
        );
    }
}

/// The guards a schedule *can* use are untouched — the refusal list is about
/// what has no meaning here, not about narrowing what a cron channel may do.
#[tokio::test]
async fn the_guards_a_schedule_can_use_are_accepted() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    let mut body = cron_channel("guarded-ok", &wf);
    body["config"] = json!({
        "timeout_ms": 1_800_000,
        "validation_logic": {"!!": {"var": "data.window"}},
        "backpressure": {"max_concurrent_per_node": 1},
        "tracing": {"mode": "sync", "task_details": true},
    });
    let (status, body) = post_channel(&app, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ============================================================
// D6: the doors a cron channel does not open
// ============================================================

/// Every channel is reachable by name at `/api/v2/data/{name}`. A cron channel
/// must not be: running it there would execute the workflow outside the
/// occurrence ledger and outside its singleton, so nothing would record that it
/// ran and nothing would stop it overlapping the scheduled run.
#[tokio::test]
async fn a_cron_channel_is_not_reachable_over_http() {
    let app = common::test_app().await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    common::deploy_channel(&app, cron_channel("unreachable", &wf)).await;

    // Both the synchronous path and the submission path.
    for path in ["/api/v2/data/unreachable", "/api/v2/data/unreachable/async"] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", path, Some(json!({}))))
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} must not run a cron channel"
        );
        let body = body_json(resp).await;
        let message = body["error"]["message"].as_str().expect("message");
        assert!(
            message.contains("cron channel") && message.contains("trigger"),
            "the refusal must point at the manual trigger: {message}"
        );
    }
}

/// D3: the one configuration whose failure an operator cannot otherwise see —
/// a deployed schedule on a node that will never run it. The deployment is
/// committed, but the node reports itself degraded on it and quarantines the
/// channel, naming the setting, rather than discovering it at the first
/// missed run.
#[tokio::test]
async fn a_cron_channel_deployed_with_the_scheduler_off_is_quarantined() {
    let mut config = orion::config::AppConfig::default();
    config.cron.enabled = false;
    let app = common::test_app_with_config(config).await;
    let wf =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Rollup")).await;

    // Authoring still works with the scheduler off: an instance that cannot run
    // schedules is still a place to write and promote them.
    let (status, body) = post_channel(&app, cron_channel("disabled", &wf)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let digest = body["digest"].as_str().expect("a digest").to_string();

    let (base, members) = deploy::merged(
        &app,
        vec![deploy::Upsert {
            kind: "channel",
            id: "disabled".into(),
            digest,
            revision: None,
            requires: vec![("workflow", wf)],
        }],
    )
    .await;
    let (status, body) = deploy::apply(&app, base, members).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(
        body["evaluation"]["degraded_nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty()),
        "the node must report itself degraded on this deployment: {body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("request");
    let health = body_json(resp).await;
    assert_eq!(health["components"]["cron"], "degraded", "{health}");
    let quarantined = health["channels"]["quarantined"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        quarantined.iter().any(|q| q["channel"] == "disabled"
            && q["reason"]
                .as_str()
                .is_some_and(|r| r.contains("cron.enabled"))),
        "the quarantine must name the setting: {health}"
    );

    // Not serving: there is no schedule a manual trigger could run.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/admin/channels/disabled/trigger",
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
