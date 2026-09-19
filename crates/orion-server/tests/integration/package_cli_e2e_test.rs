//! `orion-server package` end to end: two real server processes, an artifact
//! exported from one and promoted to the other — the documented Dev → Prod
//! promotion walkthrough as a test.
//!
//! Spawned binaries rather than `tower::oneshot`, because the CLI is an HTTP
//! client: the thing under test includes the wire.

use std::process::{Child, Command};

fn orion_bin() -> String {
    env!("CARGO_BIN_EXE_orion-server").to_string()
}

/// A scratch directory under the system temp dir, removed on drop —
/// the same pattern `cli_subcommands_test` uses, without a tempfile dep.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("orion-pkg-e2e-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A spawned server killed on drop, so a failing assertion cannot leak it.
struct Server {
    child: Child,
    port: u16,
    label: &'static str,
    log: std::path::PathBuf,
    _dir: ScratchDir,
}

impl Server {
    fn start(label: &'static str) -> Self {
        let dir = ScratchDir::new("db");
        // Bind-then-drop to pick a free port; the tiny race is acceptable in
        // a test that retries readiness anyway.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("probe port")
            .local_addr()
            .expect("addr")
            .port();
        let db = dir.path().join("e2e.db");
        // Capture the server's own output instead of discarding it. When this
        // test fails on an HTTP 500 the CLI can only report the sanitised
        // envelope ("An internal storage error occurred") — the sqlx error
        // behind it is logged server-side by errors.rs's
        // `tracing::error!(error.category = "storage", ...)`, which at the
        // `warn` level below does reach this file. Sending it to /dev/null
        // meant a CI failure named the symptom and destroyed the cause; the
        // Drop impl replays it on panic.
        let log = dir.path().join("server.log");
        let out = std::fs::File::create(&log).expect("create server log");
        let err = out.try_clone().expect("clone server log handle");
        let child = Command::new(orion_bin())
            .env(
                "ORION_STORAGE__URL",
                format!("sqlite:{}?mode=rwc", db.display()),
            )
            .env("ORION_SERVER__PORT", port.to_string())
            // Plugins on: the promotion scenario below carries one.
            .env("ORION_PLUGINS__ENABLED", "true")
            // Models on, with the admission worker this process starts:
            // the model scenario needs the target to admit what apply stages.
            .env("ORION_MODELS__ENABLED", "true")
            .env(
                "ORION_MODELS__CACHE_DIR",
                dir.path().join("models-cache").display().to_string(),
            )
            .env("ORION_LOGGING__LEVEL", "warn")
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .spawn()
            .expect("spawn orion-server");
        Self {
            child,
            port,
            label,
            log,
            _dir: dir,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn wait_ready(&self, client: &reqwest::Client) {
        for _ in 0..120 {
            if let Ok(resp) = client.get(format!("{}/readyz", self.url())).send().await
                && resp.status().is_success()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!("server on port {} never became ready", self.port);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only on the way out of a failing test: replay what the server logged
        // so the panic message and the cause land in the same CI output. This
        // runs before `_dir` is dropped (Rust runs a type's own Drop before its
        // fields'), so the scratch dir still exists to read from.
        if std::thread::panicking()
            && let Ok(text) = std::fs::read_to_string(&self.log)
        {
            let text = text.trim();
            if !text.is_empty() {
                eprintln!(
                    "--- {} server log (port {}) ---\n{}\n--- end {} server log ---",
                    self.label, self.port, text, self.label
                );
            }
        }
    }
}

fn package_cmd(args: &[&str]) -> std::process::Output {
    Command::new(orion_bin())
        .arg("package")
        .args(args)
        .output()
        .expect("invoke orion-server package")
}

#[track_caller]
fn assert_ok(out: &std::process::Output, what: &str) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{what} failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

async fn seed(client: &reqwest::Client, base: &str) {
    let post = |path: &str, body: serde_json::Value| {
        client.post(format!("{base}{path}")).json(&body).send()
    };
    let resp = post(
        "/api/v1/admin/workflows",
        serde_json::json!({
            "workflow_id": "e2e-flow", "name": "E2E Flow", "tags": ["pkg:e2e"],
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": "hi"}}}],
        }),
    )
    .await
    .expect("create workflow");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/workflows/e2e-flow/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate workflow");
    assert_eq!(resp.status(), 200);
    let resp = post(
        "/api/v1/admin/channels",
        serde_json::json!({
            "channel_id": "e2e-intake", "name": "e2e-intake", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/e2e",
            "workflow_id": "e2e-flow", "tags": ["pkg:e2e"],
        }),
    )
    .await
    .expect("create channel");
    assert_eq!(resp.status(), 201);
    let resp = client
        .patch(format!("{base}/api/v1/admin/channels/e2e-intake/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate channel");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn package_promotes_between_real_instances() {
    let client = reqwest::Client::new();
    let source = Server::start("source");
    let target = Server::start("target");
    source.wait_ready(&client).await;
    target.wait_ready(&client).await;
    seed(&client, &source.url()).await;

    let dir = ScratchDir::new("artifacts");
    let artifact = dir.path().join("e2e-1.0.0.json");
    let artifact = artifact.to_str().expect("utf8 path");

    // export → lint → plan → apply
    assert_ok(
        &package_cmd(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:e2e",
            "--name",
            "e2e",
            "--version",
            "1.0.0",
            "-o",
            artifact,
        ]),
        "export",
    );
    assert_ok(&package_cmd(&["lint", "-f", artifact]), "lint");
    assert_ok(
        &package_cmd(&["plan", "-s", &target.url(), "-f", artifact]),
        "plan",
    );
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", artifact]),
        "apply",
    );
    assert!(stdout.contains("applied e2e@1.0.0"), "{stdout}");

    // The promoted channel serves on the target.
    let resp = client
        .post(format!("{}/api/v1/data/e2e", target.url()))
        .json(&serde_json::json!({"data": {"x": 1}}))
        .send()
        .await
        .expect("data-plane request");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    // No drift; a second apply is a no-op (the receipt short-circuits it).
    assert_ok(
        &package_cmd(&["diff", "-s", &target.url(), "-f", artifact]),
        "diff",
    );
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", artifact]),
        "second apply",
    );
    assert!(stdout.contains("nothing to do"), "{stdout}");

    // The receipt records the applied version.
    let receipt: serde_json::Value = client
        .get(format!("{}/api/v1/admin/packages/e2e", target.url()))
        .send()
        .await
        .expect("receipt")
        .json()
        .await
        .expect("receipt json");
    assert_eq!(receipt["data"]["current"]["version"], "1.0.0");
    assert_eq!(receipt["data"]["current"]["state"], "applied");

    // Immutability: change the source, re-export as the SAME version, and
    // both plan and apply must refuse with the bump instruction.
    let resp = client
        .post(format!(
            "{}/api/v1/admin/workflows/e2e-flow/versions",
            source.url()
        ))
        .send()
        .await
        .expect("new version");
    assert_eq!(resp.status(), 201);
    let resp = client
        .put(format!("{}/api/v1/admin/workflows/e2e-flow", source.url()))
        .json(&serde_json::json!({
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": "v2"}}}],
        }))
        .send()
        .await
        .expect("update draft");
    assert_eq!(resp.status(), 200);
    let resp = client
        .patch(format!(
            "{}/api/v1/admin/workflows/e2e-flow/status",
            source.url()
        ))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate v2");
    assert_eq!(resp.status(), 200);

    let edited = dir.path().join("e2e-1.0.0-edited.json");
    let edited = edited.to_str().expect("utf8 path");
    assert_ok(
        &package_cmd(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:e2e",
            "--name",
            "e2e",
            "--version",
            "1.0.0",
            "-o",
            edited,
        ]),
        "re-export",
    );
    let out = package_cmd(&["plan", "-s", &target.url(), "-f", edited]);
    assert!(
        !out.status.success(),
        "a reused applied version must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bump the package version"), "{stderr}");

    // The bump promotes cleanly, and the old artifact now shows drift.
    let bumped = dir.path().join("e2e-1.1.0.json");
    let bumped = bumped.to_str().expect("utf8 path");
    assert_ok(
        &package_cmd(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:e2e",
            "--name",
            "e2e",
            "--version",
            "1.1.0",
            "-o",
            bumped,
        ]),
        "export 1.1.0",
    );
    assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", bumped]),
        "apply 1.1.0",
    );
    let out = package_cmd(&["diff", "-s", &target.url(), "-f", artifact]);
    assert!(
        !out.status.success(),
        "the superseded artifact must show drift"
    );

    // Rollback is a re-apply: 1.0.0's receipt is applied with this very
    // content, but 1.1.0 superseded it, so apply must put 1.0.0's content
    // back rather than stop at the receipt. SQLite timestamps are
    // second-granular; the touch must land strictly after 1.1.0's for
    // `current` to move back.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let stdout = assert_ok(
        &package_cmd(&["plan", "-s", &target.url(), "-f", artifact]),
        "plan the rollback",
    );
    assert!(stdout.contains("superseded by e2e@1.1.0"), "{stdout}");
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", artifact]),
        "apply the rollback",
    );
    assert!(!stdout.contains("nothing to do"), "{stdout}");
    assert!(stdout.contains("applied e2e@1.0.0"), "{stdout}");
    assert_ok(
        &package_cmd(&["diff", "-s", &target.url(), "-f", artifact]),
        "diff after the rollback",
    );
    let receipt: serde_json::Value = client
        .get(format!("{}/api/v1/admin/packages/e2e", target.url()))
        .send()
        .await
        .expect("receipt")
        .json()
        .await
        .expect("receipt json");
    assert_eq!(receipt["data"]["current"]["version"], "1.0.0", "{receipt}");
    // …and once it is current again, applying it is the no-op.
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", artifact]),
        "re-apply the current version",
    );
    assert!(stdout.contains("nothing to do"), "{stdout}");
}

/// A package with a plugin in it: the fourth member travels with its
/// component, installs on a target that has never seen it, and activates
/// before the workflow that calls it — so the promoted channel serves
/// through the plugin on the first request. Without the component the same
/// package is refused by `plan`, naming the flag that carries it.
#[tokio::test]
async fn package_promotes_a_plugin_with_its_component() {
    use base64::Engine as _;
    let client = reqwest::Client::new();
    let source = Server::start("plugin-source");
    let target = Server::start("plugin-target");
    source.wait_ready(&client).await;
    target.wait_ready(&client).await;

    let manifest = include_str!("../fixtures/plugins/fixture-upload.toml");
    let component = base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("../fixtures/plugins/fixture.wasm"));
    let base = source.url();
    let resp = client
        .post(format!("{base}/api/v1/admin/plugins"))
        .json(&serde_json::json!({"manifest": manifest, "component": component}))
        .send()
        .await
        .expect("create plugin");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/plugins/test.fixture/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate plugin");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .post(format!("{base}/api/v1/admin/workflows"))
        .json(&serde_json::json!({
            "workflow_id": "plugin-flow", "name": "Plugin Flow", "tags": ["pkg:plugin"],
            "tasks": [
                {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "input"}}},
                {"id": "wrap", "name": "wrap", "function": {"name": "test.fixture.wrap",
                    "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
            ],
        }))
        .send()
        .await
        .expect("create workflow");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/workflows/plugin-flow/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate workflow");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .post(format!("{base}/api/v1/admin/channels"))
        .json(&serde_json::json!({
            "channel_id": "plugin-intake", "name": "plugin-intake", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/plugin-intake",
            "workflow_id": "plugin-flow", "tags": ["pkg:plugin"],
        }))
        .send()
        .await
        .expect("create channel");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/channels/plugin-intake/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate channel");
    assert_eq!(resp.status(), 200);

    let dir = ScratchDir::new("plugin-artifacts");
    let thin = dir.path().join("plugin-1.0.0-thin.json");
    let thin = thin.to_str().expect("utf8 path");
    let full = dir.path().join("plugin-1.0.0.json");
    let full = full.to_str().expect("utf8 path");

    // Exported by manifest and digest only, the package lints — the manifest
    // is enough to check the workflow's input — but cannot install on a
    // target that has never held the component, and `plan` says which flag
    // would have carried it.
    assert_ok(
        &package_cmd(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:plugin",
            "--name",
            "plugin",
            "--version",
            "1.0.0",
            "-o",
            thin,
        ]),
        "thin export",
    );
    assert_ok(&package_cmd(&["lint", "-f", thin]), "thin lint");
    let out = package_cmd(&["plan", "-s", &target.url(), "-f", thin]);
    assert!(
        !out.status.success(),
        "a digest-only plugin cannot plan onto a fresh target"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--include-artifacts"), "{stderr}");

    // With the component inlined it applies: the plugin is staged and
    // activated before the workflow, and the channel serves through it.
    let stdout = assert_ok(
        &package_cmd(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:plugin",
            "--include-artifacts",
            "--name",
            "plugin",
            "--version",
            "1.0.0",
            "-o",
            full,
        ]),
        "full export",
    );
    assert!(stdout.contains("1 plugins"), "{stdout}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(full).expect("artifact")).expect("json");
    assert_eq!(artifact["plugins"][0]["plugin_id"], "test.fixture");
    assert!(artifact["plugins"][0]["component"].is_string());
    assert_eq!(artifact["plugins"][0]["activate"], true);
    assert_ok(&package_cmd(&["lint", "-f", full]), "full lint");
    assert_ok(
        &package_cmd(&["plan", "-s", &target.url(), "-f", full]),
        "full plan",
    );
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", full]),
        "apply",
    );
    assert!(
        stdout.contains("activated plugins 'test.fixture'"),
        "{stdout}"
    );
    assert!(stdout.contains("applied plugin@1.0.0"), "{stdout}");

    let resp = client
        .post(format!("{}/api/v1/data/plugin-intake", target.url()))
        .json(&serde_json::json!({"data": {"msg": "hi"}}))
        .send()
        .await
        .expect("data-plane request");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["data"]["result"]["wrapped"]["message"], "hi", "{body}");

    // No drift, and the target's catalogue names the same digest the source did.
    assert_ok(
        &package_cmd(&["diff", "-s", &target.url(), "-f", full]),
        "diff",
    );
    let functions: serde_json::Value = client
        .get(format!("{}/api/v1/admin/functions", target.url()))
        .send()
        .await
        .expect("functions")
        .json()
        .await
        .expect("json");
    let entry = functions["data"]
        .as_array()
        .expect("array")
        .iter()
        .find(|f| f["name"] == "test.fixture.wrap")
        .expect("the plugin function is served on the target");
    assert_eq!(entry["plugin"]["digest"], artifact["plugins"][0]["digest"]);
}

/// A package with a model in it: the fifth member travels as its reference,
/// the storage connector it is fetched through is a stated requirement that
/// `plan` refuses a bare target for, and `apply` waits for the target to
/// admit the model before activating it ahead of the workflow — so the
/// promoted channel scores a board on the first request.
#[tokio::test]
async fn package_promotes_a_model_by_reference_and_waits_for_admission() {
    use crate::common::models::{
        FIXTURE_ID, FIXTURE_ONNX, fixture_digest, registration, spawn_bucket, storage_connector,
    };
    /// The CLI on the blocking pool: the bucket both servers fetch from is a
    /// task on this test's runtime, and a `Command::output()` on the runtime
    /// thread would stall it exactly while the target's admission needs it.
    async fn package(args: &[&str]) -> std::process::Output {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            package_cmd(&borrowed)
        })
        .await
        .expect("the CLI ran")
    }
    let client = reqwest::Client::new();
    let bucket = spawn_bucket(FIXTURE_ONNX.to_vec()).await;
    let source = Server::start("model-source");
    let target = Server::start("model-target");
    source.wait_ready(&client).await;
    target.wait_ready(&client).await;
    let base = source.url();

    // The source: a storage connector at the bucket, the model registered,
    // admitted by the source's own worker and activated, and a workflow
    // calling it behind a channel.
    let resp = client
        .post(format!("{base}/api/v1/admin/connectors"))
        .json(&storage_connector("bucket", bucket.addr))
        .send()
        .await
        .expect("create connector");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .post(format!("{base}/api/v1/admin/models"))
        .json(&registration("bucket", &fixture_digest()))
        .send()
        .await
        .expect("register model");
    assert_eq!(
        resp.status(),
        202,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let mut admitted = false;
    for _ in 0..240 {
        let row: serde_json::Value = client
            .get(format!("{base}/api/v1/admin/models/{FIXTURE_ID}"))
            .send()
            .await
            .expect("get model")
            .json()
            .await
            .expect("json");
        match row["data"]["admission"]["state"].as_str() {
            Some("passed") => {
                admitted = true;
                break;
            }
            Some("failed") => panic!("the source refused the fixture: {row}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
    assert!(admitted, "the source's admission worker never ran");
    let resp = client
        .patch(format!("{base}/api/v1/admin/models/{FIXTURE_ID}/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate model");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .post(format!("{base}/api/v1/admin/workflows"))
        .json(&serde_json::json!({
            "workflow_id": "score", "name": "Score", "tags": ["pkg:model"], "condition": true,
            "tasks": [
                {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "board"}}},
                {"id": "infer", "name": "infer", "function": {"name": "model_infer",
                    "input": {"model": FIXTURE_ID, "input": {"var": ""}, "output": "data.policy"}}}
            ],
        }))
        .send()
        .await
        .expect("create workflow");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/workflows/score/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate workflow");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .post(format!("{base}/api/v1/admin/channels"))
        .json(&serde_json::json!({
            "channel_id": "score-api", "name": "score-api", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/score",
            "workflow_id": "score", "tags": ["pkg:model"],
        }))
        .send()
        .await
        .expect("create channel");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let resp = client
        .patch(format!("{base}/api/v1/admin/channels/score-api/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate channel");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );

    let dir = ScratchDir::new("model-artifacts");
    let file = dir.path().join("score-1.0.0.json");
    let file = file.to_str().expect("utf8 path");
    let stdout = assert_ok(
        &package(&[
            "export",
            "-s",
            &source.url(),
            "--tag",
            "pkg:model",
            "--name",
            "score",
            "--version",
            "1.0.0",
            "-o",
            file,
        ])
        .await,
        "export",
    );
    assert!(stdout.contains("1 models"), "{stdout}");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).expect("artifact")).expect("json");
    assert_eq!(artifact["models"][0]["model_id"], FIXTURE_ID);
    assert_eq!(artifact["models"][0]["artifact"]["connector"], "bucket");
    assert_eq!(
        artifact["models"][0]["artifact"]["digest"],
        serde_json::json!(fixture_digest())
    );
    assert_eq!(artifact["models"][0]["activate"], true);
    assert!(artifact["models"][0].get("component").is_none());
    assert_eq!(
        artifact["requires"]["storage"],
        serde_json::json!(["bucket"])
    );
    assert_ok(&package(&["lint", "-f", file]).await, "lint");

    // A target without the storage connector cannot take the package, and
    // `plan` names the connector before anything is written.
    let out = package(&["plan", "-s", &target.url(), "-f", file]).await;
    assert!(!out.status.success(), "a bare target lacks the connector");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("required storage connector 'bucket'"),
        "{stderr}"
    );
    let out = package(&["apply", "-s", &target.url(), "-f", file]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("'bucket'"), "{stderr}");

    // With the connector in place: plan, apply — which waits for the
    // target's admission — and the channel serves the model.
    let resp = client
        .post(format!("{}/api/v1/admin/connectors", target.url()))
        .json(&storage_connector("bucket", bucket.addr))
        .send()
        .await
        .expect("create connector");
    assert_eq!(
        resp.status(),
        201,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    assert_ok(
        &package(&["plan", "-s", &target.url(), "-f", file]).await,
        "plan",
    );
    let stdout = assert_ok(
        &package(&["apply", "-s", &target.url(), "-f", file]).await,
        "apply",
    );
    assert!(stdout.contains("staged models: 1 written"), "{stdout}");
    assert!(stdout.contains("admitted models 'ada.c4-tiny'"), "{stdout}");
    assert!(stdout.contains("1479 parameters"), "{stdout}");
    assert!(
        stdout.contains("activated models 'ada.c4-tiny'"),
        "{stdout}"
    );
    assert!(stdout.contains("applied score@1.0.0"), "{stdout}");
    assert!(
        bucket.gets.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "each instance fetched the artifact through its own connector"
    );

    let mut planes = vec![vec![vec![0.0f32; 7]; 6]; 2];
    planes[0][5][3] = 1.0;
    let resp = client
        .post(format!("{}/api/v1/data/score", target.url()))
        .json(&serde_json::json!({"data": [planes]}))
        .send()
        .await
        .expect("data-plane request");
    assert_eq!(
        resp.status(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        body["data"]["policy"]["policy"][0].as_array().map(Vec::len),
        Some(7),
        "{body}"
    );

    // No drift, and re-applying identical content is a no-op.
    assert_ok(
        &package(&["diff", "-s", &target.url(), "-f", file]).await,
        "diff",
    );
    let stdout = assert_ok(
        &package(&["apply", "-s", &target.url(), "-f", file]).await,
        "re-apply",
    );
    assert!(stdout.contains("nothing to do"), "{stdout}");
}
