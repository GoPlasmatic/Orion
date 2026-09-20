//! `[packages] apply`: the artifacts a node applies to itself at startup,
//! through `orion::package::boot::run` over the node's own admin router.
//! `/readyz` holds at 503 until they serve, a failure is recorded where
//! `main` turns it into an exit, and a restart is a no-op.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{body_json, json_request, test_state_with_config};
use crate::package_in_process_test::artifact;
use orion::package::artifact::{PackageArtifact, artifact_content_hash};
use orion::runtime::boot_packages::BootState;
use orion::server::state::AppState;

/// A scratch directory removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("orion-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }

    fn write(&self, name: &str, artifact: &PackageArtifact) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, serde_json::to_vec(artifact).expect("json")).expect("write");
        path.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn node(files: Vec<String>) -> (AppState, axum::Router) {
    let mut config = orion::config::AppConfig::default();
    config.packages.apply = files;
    let state = test_state_with_config(config).await;
    let router = orion::server::build_router(state.clone());
    (state, router)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request("GET", uri, None))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn readyz_holds_until_the_packages_serve_and_a_restart_is_a_no_op() {
    let scratch = Scratch::new();
    let file = scratch.write("orders.json", &artifact("boot", "1.0.0", "/boot"));
    let (state, app) = node(vec![file.clone()]).await;

    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["components"]["packages"], "applying");

    orion::package::boot::run(state.clone()).await;
    let entry = &state.packages.snapshot()[0];
    assert_eq!(entry.state, BootState::Applied, "{:?}", entry.error);
    assert_eq!(entry.name, "boot");
    assert_eq!(entry.version, "1.0.0");

    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"]["packages"], "ok");
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/boot",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("data call");
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, receipt) = get(&app, "/api/v1/admin/packages/boot").await;
    assert_eq!(
        receipt["data"]["current"]["principal"],
        "system:boot-packages"
    );
    let (_, health) = get(&app, "/health").await;
    assert_eq!(health["components"]["packages"], "ok");
    assert_eq!(health["packages"][0]["state"], "applied", "{health}");

    // The same artifact again, as a restart runs it: nothing written.
    orion::package::boot::run(state.clone()).await;
    assert_eq!(
        state.packages.snapshot()[0].state,
        BootState::AlreadyApplied
    );
    let (_, versions) = get(&app, "/api/v1/admin/workflows/boot-flow/versions").await;
    assert_eq!(
        versions["data"].as_array().map(Vec::len),
        Some(1),
        "{versions}"
    );
}

#[tokio::test]
async fn no_packages_leaves_the_probes_as_they_were() {
    let (state, app) = node(Vec::new()).await;
    orion::package::boot::run(state).await;
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["components"].get("packages").is_none(), "{body}");
}

#[tokio::test]
async fn an_artifact_that_does_not_check_fails_before_anything_is_written() {
    let scratch = Scratch::new();
    let good = scratch.write("good.json", &artifact("first", "1.0.0", "/first"));
    let mut tampered = artifact("second", "1.0.0", "/second");
    tampered.package.content_hash = "sha256:0000".to_string();
    let bad = scratch.write("bad.json", &tampered);
    let (state, app) = node(vec![good, bad]).await;

    orion::package::boot::run(state.clone()).await;
    let failure = state.packages.failure().expect("failed");
    assert!(failure.contains("packages.apply[1]"), "{failure}");
    assert!(failure.contains("nothing was applied"), "{failure}");
    tokio::time::timeout(std::time::Duration::from_secs(1), state.packages.failed())
        .await
        .expect("the failure wakes main");
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["components"]["packages"], "failed");
    // Not even the first, valid artifact was applied.
    let (status, _) = get(&app, "/api/v1/admin/packages/first").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Applied is not serving: a carried connector whose reference does not
/// resolve on this node is quarantined, and the boot fails naming it.
#[tokio::test]
async fn a_package_the_node_quarantines_fails_the_boot() {
    let scratch = Scratch::new();
    let mut quarantined = artifact("crm", "1.0.0", "/crm");
    quarantined.connectors = vec![json!({
        "name": "crm-api", "connector_type": "http",
        "config": {"type": "http", "url": "env://ORION_BOOT_TEST_NEVER_SET_URL"},
    })];
    quarantined.package.content_hash = artifact_content_hash(&quarantined).expect("hash");
    let file = scratch.write("crm.json", &quarantined);
    let (state, _) = node(vec![file]).await;

    orion::package::boot::run(state.clone()).await;
    let failure = state.packages.failure().expect("failed");
    assert!(failure.contains("failed to apply at startup"), "{failure}");
    assert!(failure.contains("not serving"), "{failure}");
    assert_eq!(state.packages.snapshot()[0].state, BootState::Failed);
}

/// A node restarting on an older artifact than the one applied since does
/// not roll the package back.
#[tokio::test]
async fn a_superseded_version_is_left_as_it_is() {
    let scratch = Scratch::new();
    let old = artifact("rolling", "1.0.0", "/rolling");
    let file = scratch.write("old.json", &old);
    let (state, app) = node(vec![file]).await;
    orion::package::boot::run(state.clone()).await;
    assert_eq!(state.packages.snapshot()[0].state, BootState::Applied);

    // A newer deploy applies 2.0.0, with different content.
    let mut new = artifact("rolling", "2.0.0", "/rolling");
    new.workflows[0]["tasks"][0]["function"]["input"]["message"] = json!("v2");
    new.package.content_hash = artifact_content_hash(&new).expect("hash");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let api = orion::package::InProcessAdmin::new(state.clone(), "test", "test".to_string());
    let opts = orion::package::ApplyOptions {
        prune: None,
        signed: &[],
        roll_back_superseded: true,
    };
    orion::package::apply(&api, "this node", &new, &opts, &orion::package::Console)
        .await
        .expect("apply 2.0.0");

    // The old node restarts.
    orion::package::boot::run(state.clone()).await;
    assert_eq!(state.packages.snapshot()[0].state, BootState::Superseded);
    let (_, receipt) = get(&app, "/api/v1/admin/packages/rolling").await;
    assert_eq!(receipt["data"]["current"]["version"], "2.0.0");
    let (_, workflow) = get(&app, "/api/v1/admin/workflows/rolling-flow").await;
    assert_eq!(
        workflow["data"]["tasks"][0]["function"]["input"]["message"], "v2",
        "{workflow}"
    );
}
