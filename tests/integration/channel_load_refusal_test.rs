//! N3/N4: a channel whose stored config no longer parses, or whose
//! `validation_logic` no longer compiles, must refuse to load rather than
//! serve with its guards silently absent — on a single node, not just in
//! cluster mode.
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

    let (status, body) = reload_engine(&app).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "reload must fail, got body {body}"
    );
    assert_eq!(body["error"]["code"], json!("CONFIG_ERROR"));
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("refused to load") && message.contains(bad_channel),
        "unexpected message: {message}"
    );
    assert!(
        message.contains(reason_fragment),
        "expected reason {reason_fragment:?} in: {message}"
    );

    // The refused channel must not be reachable...
    assert!(
        state
            .channel_registry
            .get_by_name(bad_channel)
            .await
            .is_none(),
        "refused channel must not be in the registry"
    );
    // ...and the refusal must not have partially applied, dropping the
    // channels that were serving fine.
    assert!(
        state
            .channel_registry
            .get_by_name("keep-ch")
            .await
            .is_some(),
        "a refusal must leave previously loaded channels in place"
    );
    assert!(serves_ok(&app, "keep-ch").await);
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
