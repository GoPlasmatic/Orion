//! The v1 entity routes, for the tests whose subject is v1 itself —
//! draft/active/archived lifecycle, v1 row versioning, import/export, the v1
//! audit actions and DTOs. They go when those routes go.
//!
//! Everything else deploys through [`super::deploy`]: once any deployment
//! exists a node builds only from the manifest, so a row these helpers
//! create in a test that has also deployed is stored but never served.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{body_json, json_request};

/// `POST` `body` to a v1 create route, answering the created `data`.
async fn create(app: &Router, uri: &str, body: Value) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request("POST", uri, Some(body)))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "{uri}: {body}");
    body["data"].clone()
}

/// Activate a v1 workflow or channel (`kind` is the route segment).
pub async fn activate(app: &Router, kind: &str, id: &str) {
    let uri = format!("/api/v2/admin/{kind}/{id}/status");
    let resp = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &uri,
            Some(json!({"status": "active"})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
}

/// Create a v1 connector, answering its id.
pub async fn create_connector(app: &Router, connector: Value) -> String {
    let data = create(app, "/api/v2/admin/connectors", connector).await;
    data["id"].as_str().unwrap().to_string()
}

/// Create and activate a v1 workflow, answering its id.
pub async fn create_and_activate_workflow(app: &Router, workflow: Value) -> String {
    let data = create(app, "/api/v2/admin/workflows", workflow).await;
    let id = data["workflow_id"].as_str().unwrap().to_string();
    activate(app, "workflows", &id).await;
    id
}

/// Create and activate a v1 channel from its full body, answering its id.
pub async fn create_and_activate_channel_body(app: &Router, channel: Value) -> String {
    let data = create(app, "/api/v2/admin/channels", channel).await;
    let id = data["channel_id"].as_str().unwrap().to_string();
    activate(app, "channels", &id).await;
    id
}

/// A v1 workflow and a sync HTTP channel at `/{name}` bound to it, both
/// active, answering `(channel_id, workflow_id)`.
pub async fn create_and_activate_channel_full(
    app: &Router,
    channel_name: &str,
    workflow: Value,
    channel_config: Value,
) -> (String, String) {
    let workflow_id = create_and_activate_workflow(app, workflow).await;
    let channel_id = create_and_activate_channel_body(
        app,
        json!({
            "name": channel_name,
            "channel_type": "sync",
            "protocol": "http",
            "methods": ["POST"],
            "route_pattern": format!("/{channel_name}"),
            "workflow_id": workflow_id,
            "config": channel_config,
        }),
    )
    .await;
    (channel_id, workflow_id)
}

/// [`create_and_activate_channel_full`] with an empty config, answering
/// `(channel_name, workflow_id)`.
pub async fn create_and_activate_channel(
    app: &Router,
    channel_name: &str,
    workflow: Value,
) -> (String, String) {
    let (_, workflow_id) =
        create_and_activate_channel_full(app, channel_name, workflow, json!({})).await;
    (channel_name.to_string(), workflow_id)
}

/// Create and activate a v1 REST channel bound to `workflow_id`, answering
/// its id.
pub async fn create_rest_channel(
    app: &Router,
    name: &str,
    route_pattern: &str,
    methods: Vec<&str>,
    workflow_id: &str,
) -> String {
    create_and_activate_channel_body(
        app,
        json!({
            "name": name,
            "channel_type": "sync",
            "protocol": "rest",
            "methods": methods,
            "route_pattern": route_pattern,
            "workflow_id": workflow_id,
        }),
    )
    .await
}

/// [`create_and_activate_channel_full`], answering `(channel_name,
/// workflow_id)`.
pub async fn create_and_activate_channel_with_config(
    app: &Router,
    channel_name: &str,
    workflow: Value,
    channel_config: Value,
) -> (String, String) {
    let (_, workflow_id) =
        create_and_activate_channel_full(app, channel_name, workflow, channel_config).await;
    (channel_name.to_string(), workflow_id)
}

/// A v1 HTTP connector to `addr` with zero retries and private URLs allowed.
pub async fn create_http_connector(app: &Router, name: &str, addr: std::net::SocketAddr) {
    create_connector(
        app,
        json!({
            "id": name,
            "name": name,
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{addr}"),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;
}
