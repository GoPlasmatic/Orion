//! `DELETE …?reload=defer` on the four versioned kinds: the row goes, and
//! the published generation does not move until `POST /engine/reload` —
//! what lets `package apply --prune=delete` remove any number of entities
//! inside the apply's one reload. Without the parameter a delete still
//! republishes.

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::models::{harness, register_fixture};
use crate::common::{
    create_and_activate_channel_full, echo_workflow, json_request, test_state_with_config,
};
use orion::server::state::AppState;

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(json_request(method, path, body))
        .await
        .expect("request");
    resp.status()
}

/// Delete `path` with the reload deferred: `204`, the row gone, nothing
/// published.
async fn delete_deferred(state: &AppState, app: &axum::Router, path: &str) {
    let before = state.runtime.published_count();
    assert_eq!(
        send(app, "DELETE", &format!("{path}?reload=defer"), None).await,
        StatusCode::NO_CONTENT,
        "{path}"
    );
    assert_eq!(
        state.runtime.published_count(),
        before,
        "a deferred delete of {path} must not publish a generation"
    );
    assert_eq!(
        send(app, "GET", path, None).await,
        StatusCode::NOT_FOUND,
        "the row is gone"
    );
}

#[tokio::test]
async fn channel_and_workflow_deletes_defer_to_the_next_reload() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let (channel_id, workflow_id) =
        create_and_activate_channel_full(&app, "defer-ch", echo_workflow("Defer"), json!({})).await;

    delete_deferred(
        &state,
        &app,
        &format!("/api/v1/admin/channels/{channel_id}"),
    )
    .await;
    // Still served: the generation is the one published before the delete.
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("defer-ch")
            .is_some()
    );

    delete_deferred(
        &state,
        &app,
        &format!("/api/v1/admin/workflows/{workflow_id}"),
    )
    .await;

    let before = state.runtime.published_count();
    assert_eq!(
        send(&app, "POST", "/api/v1/admin/engine/reload", None).await,
        StatusCode::OK
    );
    assert_eq!(state.runtime.published_count(), before + 1);
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("defer-ch")
            .is_none()
    );
}

#[tokio::test]
async fn a_delete_without_the_parameter_still_reloads() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let (channel_id, _) =
        create_and_activate_channel_full(&app, "now-ch", echo_workflow("Now"), json!({})).await;
    let before = state.runtime.published_count();
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/api/v1/admin/channels/{channel_id}"),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(state.runtime.published_count(), before + 1);
    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("now-ch")
            .is_none()
    );
}

#[tokio::test]
async fn a_plugin_delete_defers() {
    let mut config = orion::config::AppConfig::default();
    config.plugins.enabled = true;
    config.plugins.max_timeout_ms = 2_000;
    let state = test_state_with_config(config).await;
    let app = orion::server::build_router(state.clone());
    let upload = json!({
        "manifest": include_str!("../fixtures/plugins/fixture-upload.toml"),
        "component": base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../fixtures/plugins/fixture.wasm")),
    });
    assert_eq!(
        send(&app, "POST", "/api/v1/admin/plugins", Some(upload)).await,
        StatusCode::CREATED
    );
    delete_deferred(&state, &app, "/api/v1/admin/plugins/test.fixture").await;
}

#[tokio::test]
async fn a_model_delete_defers() {
    let h = harness().await;
    let model = register_fixture(&h.app).await;
    let id = model["model_id"].as_str().expect("model id");
    delete_deferred(&h.state, &h.app, &format!("/api/v1/admin/models/{id}")).await;
}
