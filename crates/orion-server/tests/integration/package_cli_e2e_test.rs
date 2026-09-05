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
