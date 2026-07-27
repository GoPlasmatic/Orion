//! N3/N4: a channel whose stored config no longer parses, or whose
//! `validation_logic` no longer compiles, must refuse to load rather than
//! serve with its guards silently absent — on a single node, not just in
//! cluster mode.
//!
//! F35 changed *how* it is refused, not whether. The channel is quarantined:
//! absent from the registry and the route table, and refused at every ingress
//! with a 503. The reload itself succeeds, so one broken row no longer fails
//! every activate, archive, delete and rollout on the instance.
//!
//! Both rows are written straight to the DB: that is exactly how such a row
//! occurs in practice (import, a hand-edited row, or a datalogic upgrade that
//! changed what compiles), since create/update validation rejects them.

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

/// Insert an `active` channel row directly, bypassing admin validation.
async fn insert_raw_active_channel(
    state: &orion::server::state::AppState,
    name: &str,
    config: &str,
) {
    let sql = format!(
        "INSERT INTO channels (channel_id, version, name, channel_type, protocol, \
         transport_config_json, config_json, status, priority) \
         VALUES ('ch_{name}', 1, '{name}', 'sync', 'http', '{{}}', '{config}', 'active', 0)"
    );
    state
        .db_pool
        .execute_query(
            &sql,
            sea_query_binder::SqlxValues(sea_query::Values(vec![])),
        )
        .await
        .expect("raw channel insert");
}

async fn reload_engine(app: &axum::Router) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/engine/reload", None))
        .await
        .expect("reload request");
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn serves_ok(app: &axum::Router, channel: &str) -> bool {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .expect("data request");
    resp.status() == StatusCode::OK
}

async fn assert_refused(config_json: &str, bad_channel: &str, reason_fragment: &str) {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel(&app, "keep-ch", common::simple_log_workflow("Keep WF"))
        .await;
    assert!(serves_ok(&app, "keep-ch").await, "baseline channel serves");

    insert_raw_active_channel(&state, bad_channel, config_json).await;

    // F35: the reload succeeds. One broken row must not fail every admin
    // mutation that triggers a reload.
    let (status, body) = reload_engine(&app).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "reload must succeed and quarantine the broken channel, got body {body}"
    );

    // The broken channel must not be reachable...
    assert!(
        state
            .channel_registry
            .get_by_name(bad_channel)
            .await
            .is_none(),
        "quarantined channel must not be in the registry"
    );
    let reason = state
        .channel_registry
        .quarantine_reason(bad_channel)
        .await
        .expect("the broken channel must be quarantined, not merely absent");
    assert!(
        reason.contains(reason_fragment),
        "expected reason {reason_fragment:?} in: {reason}"
    );

    // ...and requesting it must be refused, not served with its guards
    // silently absent. That distinction is the whole point of N3/N4: a plain
    // registry miss would fall through to "no config, no guards".
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/data/{bad_channel}"),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .expect("data request");
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a quarantined channel must be refused at ingress"
    );
    let body = body_json(resp).await;
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains(bad_channel) && message.contains("failed to load"),
        "the refusal must say which channel and why: {message}"
    );

    // The quarantine must be confined to the broken row.
    assert!(
        state
            .channel_registry
            .get_by_name("keep-ch")
            .await
            .is_some(),
        "quarantining one channel must leave the others loaded"
    );
    assert!(serves_ok(&app, "keep-ch").await);

    // /health is the operator-facing signal that something is not being served.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .expect("health request");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "quarantine is degraded, not down"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["components"]["channels"], "degraded");
    let quarantined = body["channels"]["quarantined"].as_array().expect("array");
    assert!(
        quarantined.iter().any(|q| q["channel"] == bad_channel),
        "health must list the quarantined channel: {quarantined:?}"
    );
}

#[tokio::test]
async fn malformed_config_json_refuses_channel_load_on_single_node() {
    // Not JSON at all.
    assert_refused("{ not json", "bad-cfg-ch", "config_json does not parse").await;
}

#[tokio::test]
async fn wrongly_typed_config_field_refuses_channel_load() {
    // Valid JSON, wrong type — the proposal's example: one typo used to load
    // the channel with no rate limit, validation, dedup, or backpressure.
    assert_refused(
        r#"{"rate_limit": {"requests_per_second": "100"}}"#,
        "typo-cfg-ch",
        "config_json does not parse",
    )
    .await;
}

#[tokio::test]
async fn uncompilable_validation_logic_refuses_channel_load() {
    // Valid JSON that the datalogic compiler rejects (multi-key object).
    // The channel must not serve unvalidated because its input contract
    // stopped compiling.
    assert_refused(
        r#"{"validation_logic": {"==": [1, 1], "!=": [1, 2]}}"#,
        "bad-logic-ch",
        "validation_logic does not compile",
    )
    .await;
}

/// F35: the defect this change fixes. One channel with an unparseable
/// `config_json` used to fail *every* operation that triggers a reload —
/// activate, archive, delete, rollout — with a 500 `CONFIG_ERROR`, because the
/// registry reload was all-or-nothing and the admin handler turned a non-empty
/// issue list into a hard error. The instance became unmanageable through the
/// API until someone edited the row out of the database by hand.
#[tokio::test]
async fn a_broken_channel_does_not_wedge_admin_mutations() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let (_, keep_wf) = common::create_and_activate_channel(
        &app,
        "keep-ch",
        common::simple_log_workflow("Keep WF"),
    )
    .await;
    insert_raw_active_channel(&state, "wedge-ch", "{ not json").await;

    let (status, _) = reload_engine(&app).await;
    assert_eq!(status, StatusCode::OK);

    // Create a second channel on the same workflow so we hold its real
    // channel_id, then drive it through both reload-triggering transitions.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "new-ch",
                "channel_type": "sync",
                "protocol": "http",
                "methods": ["POST"],
                "route_pattern": "/new-ch",
                "workflow_id": keep_wf,
            })),
        ))
        .await
        .expect("create channel");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let new_id = body_json(resp).await["data"]["channel_id"]
        .as_str()
        .expect("channel_id")
        .to_string();

    // Activating triggers a reload — this is the operation that used to 500.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{new_id}/status"),
            Some(json!({"status": "active"})),
        ))
        .await
        .expect("activate request");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "activating must not be blocked by an unrelated broken channel"
    );
    assert!(
        serves_ok(&app, "new-ch").await,
        "a channel activated while another is quarantined must serve"
    );

    // Archiving triggers a reload too.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/channels/{new_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .expect("archive request");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "archiving must not be blocked by an unrelated broken channel"
    );

    // The broken channel is still quarantined throughout — the fix is about
    // blast radius, not about relaxing the refusal.
    assert!(
        state
            .channel_registry
            .quarantine_reason("wedge-ch")
            .await
            .is_some()
    );
}

/// A quarantined channel's REST route must not resolve either — otherwise it
/// would shadow the path and 503 requests that a working channel could serve.
#[tokio::test]
async fn a_quarantined_channel_is_absent_from_the_route_table() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    // Raw insert with a REST route pattern and an unparseable config.
    let sql = "INSERT INTO channels (channel_id, version, name, channel_type, protocol, \
               methods, route_pattern, transport_config_json, config_json, status, priority) \
               VALUES ('ch_route', 1, 'route-ch', 'sync', 'rest', '[\"GET\"]', \
               '/quarantined/{id}', '{}', '{ not json', 'active', 0)";
    state
        .db_pool
        .execute_query(sql, sea_query_binder::SqlxValues(sea_query::Values(vec![])))
        .await
        .expect("raw channel insert");

    let (status, _) = reload_engine(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        state
            .channel_registry
            .match_route("GET", "quarantined/42")
            .await
            .is_none(),
        "a quarantined channel must not own a route"
    );
}

// ============================================================
// F33: engine-build failures quarantine the channel
// ============================================================

/// Archiving a workflow out from under an active channel used to leave the
/// channel in the route table with no workflow behind it — requests got an
/// opaque engine error. Engine-build failures now feed the same quarantine
/// as config-load failures.
#[tokio::test]
async fn channel_whose_workflow_disappears_is_quarantined() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let (_ch_name, wf_id) = common::create_and_activate_channel(
        &app,
        "f33-orphan",
        common::simple_log_workflow("F33 Orphan WF"),
    )
    .await;

    // Serving normally before.
    assert!(serves_ok(&app, "f33-orphan").await);

    // Archive the workflow — the status change triggers a reload.
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/admin/workflows/{wf_id}/status"),
            Some(json!({"status": "archived"})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "archive must succeed (F35)");

    // The channel is quarantined: refused with 503, not an opaque engine error…
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f33-orphan",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "quarantined channel must be refused (F33)"
    );

    // …and reported on /health with the reason.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let quarantined = body["channels"]["quarantined"]
        .as_array()
        .expect("quarantined list");
    let entry = quarantined
        .iter()
        .find(|e| e["channel"] == "f33-orphan")
        .unwrap_or_else(|| panic!("f33-orphan must be quarantined, got {body}"));
    assert!(
        entry["reason"].as_str().unwrap().contains("not found"),
        "reason must name the missing workflow, got {entry}"
    );
}
