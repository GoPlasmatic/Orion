//! The apply engine through `InProcessAdmin`: the server's own admin router,
//! no socket and no admin key — what a node applying its configured packages
//! at startup runs. The same sequence as `orion-server package apply`, so
//! the receipt, the audit trail and the data plane must read the same.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{body_json, json_request, test_state_with_config, wait_for_body};
use orion::package::artifact::{PackageArtifact, PackageMeta, Requires, artifact_content_hash};
use orion::package::{ApplyOptions, ApplyOutcome, Console, InProcessAdmin};

/// A one-channel package, hashed.
pub(crate) fn artifact(name: &str, version: &str, route: &str) -> PackageArtifact {
    let mut artifact = PackageArtifact {
        package: PackageMeta {
            name: name.to_string(),
            version: version.to_string(),
            orion: String::new(),
            content_hash: String::new(),
            exported_from: String::new(),
            exported_at: String::new(),
        },
        requires: Requires::default(),
        plugins: Vec::new(),
        models: Vec::new(),
        connectors: Vec::new(),
        workflows: vec![json!({
            "workflow_id": format!("{name}-flow"), "name": format!("{name}-flow"),
            "activate": true,
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": "hi"}}}],
        })],
        channels: vec![json!({
            "channel_id": format!("{name}-in"), "name": format!("{name}-in"),
            "channel_type": "sync", "protocol": "rest", "methods": ["POST"],
            "route_pattern": route, "workflow_id": format!("{name}-flow"), "activate": true,
        })],
    };
    artifact.package.content_hash = artifact_content_hash(&artifact).expect("hash");
    artifact
}

#[tokio::test]
async fn an_in_process_apply_serves_and_is_recorded_like_a_cli_one() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let api = InProcessAdmin::new(
        state.clone(),
        "system:test",
        "package=inproc@1.0.0 test".to_string(),
    );
    let artifact = artifact("inproc", "1.0.0", "/inproc");
    let opts = ApplyOptions {
        prune: None,
        signed: &[],
        roll_back_superseded: false,
    };

    let outcome = orion::package::apply(&api, "this node", &artifact, &opts, &Console)
        .await
        .expect("apply");
    assert_eq!(outcome, ApplyOutcome::Applied);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/inproc",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("data call");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/packages/inproc", None))
        .await
        .expect("receipt");
    let receipt: Value = body_json(resp).await;
    assert_eq!(receipt["data"]["current"]["version"], "1.0.0");
    assert_eq!(receipt["data"]["current"]["principal"], "system:test");
    assert_eq!(
        receipt["data"]["current"]["inventory"]["channels"],
        json!(["inproc-in"])
    );

    // The audit rows carry the change context the transport scoped.
    let audit = wait_for_body(
        &app,
        "/api/v1/admin/audit-logs?resource_type=package",
        |body| body["data"].as_array().is_some_and(|rows| !rows.is_empty()),
    )
    .await;
    let details: Value = serde_json::from_str(
        audit["data"][0]["details"]
            .as_str()
            .expect("details recorded"),
    )
    .expect("details json");
    assert_eq!(details["change_context"], "package=inproc@1.0.0 test");

    // The same artifact again: nothing written.
    let again = orion::package::apply(&api, "this node", &artifact, &opts, &Console)
        .await
        .expect("re-apply");
    assert_eq!(again, ApplyOutcome::AlreadyApplied);
}

/// A failure reads as the CLI prints it: `HTTP <status> <code>: …`.
#[tokio::test]
async fn an_in_process_refusal_reads_like_an_http_one() {
    use orion::package::AdminApi;
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let api = InProcessAdmin::new(state, "system:test", "test".to_string());
    let err = api
        .get_data::<Value>("/api/v1/admin/workflows/no-such-workflow")
        .await
        .expect_err("404");
    assert_eq!(err.status(), Some(StatusCode::NOT_FOUND));
    assert!(err.to_string().starts_with("HTTP 404 "), "{err}");
    assert!(
        api.get_data_opt::<Value>("/api/v1/admin/workflows/no-such-workflow")
            .await
            .expect("opt")
            .is_none()
    );
}
