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
    /// Holds the database; `None` only while a restart moves it on.
    dir: Option<ScratchDir>,
}

impl Server {
    fn start(label: &'static str) -> Self {
        Self::start_with(label, &[])
    }

    /// A server with extra `ORION_*` settings in its environment.
    fn start_with(label: &'static str, envs: &[(&str, &str)]) -> Self {
        Self::spawn(label, ScratchDir::new("db"), envs)
    }

    /// Stop this server and start another on the same database, with
    /// `envs` — a node whose configuration changed under a stored estate.
    fn restart_with(mut self, envs: &[(&str, &str)]) -> Self {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let dir = self.dir.take().expect("the database dir");
        Self::spawn(self.label, dir, envs)
    }

    fn spawn(label: &'static str, dir: ScratchDir, envs: &[(&str, &str)]) -> Self {
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
            .envs(envs.iter().copied())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .spawn()
            .expect("spawn orion-server");
        Self {
            child,
            port,
            label,
            log,
            dir: Some(dir),
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
        // runs before `dir` is dropped (Rust runs a type's own Drop before its
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

/// #339's motivating case: a package compiled with `--version content`
/// goes A → B → (revert) → A. The revert compiles to A's own version, whose
/// receipt is applied with that content — and superseded — so the third
/// apply must put A's content back rather than report "nothing to do".
#[tokio::test]
async fn a_content_versioned_revert_rolls_back() {
    let client = reqwest::Client::new();
    let target = Server::start("target");
    target.wait_ready(&client).await;

    let defs = ScratchDir::new("content-defs");
    let write_workflow = |message: &str| {
        std::fs::write(
            defs.path().join("wf.json"),
            serde_json::json!({
                "workflow_id": "cv-flow", "name": "CV Flow",
                "tasks": [{"id": "t1", "name": "log",
                           "function": {"name": "log", "input": {"message": message}}}],
            })
            .to_string(),
        )
        .expect("write workflow");
    };
    std::fs::write(
        defs.path().join("ch.json"),
        serde_json::json!({
            "channel_id": "cv-intake", "name": "cv-intake", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/cv",
            "workflow_id": "cv-flow",
        })
        .to_string(),
    )
    .expect("write channel");
    let out = ScratchDir::new("content-artifacts");
    let compile = |file: &str| -> (String, String) {
        let path = out.path().join(file);
        let path = path.to_str().expect("utf8").to_string();
        let result = Command::new(orion_bin())
            .args([
                "compile",
                defs.path().to_str().expect("utf8"),
                "--name",
                "cv",
                "--version",
                "content",
                "-o",
                &path,
            ])
            .output()
            .expect("compile");
        assert_ok(&result, "compile");
        let artifact: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("artifact")).expect("json");
        let version = artifact["package"]["version"]
            .as_str()
            .expect("version")
            .to_string();
        (path, version)
    };
    let current = || async {
        let receipt: serde_json::Value = client
            .get(format!("{}/api/v1/admin/packages/cv", target.url()))
            .send()
            .await
            .expect("receipt")
            .json()
            .await
            .expect("receipt json");
        receipt["data"]["current"]["version"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    write_workflow("a");
    let (a, version_a) = compile("a.json");
    assert!(version_a.starts_with("content-"), "{version_a}");
    assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &a]),
        "apply A",
    );

    write_workflow("b");
    let (b, version_b) = compile("b.json");
    assert_ne!(version_a, version_b);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &b]),
        "apply B",
    );
    assert_eq!(current().await, version_b);

    // The revert: same content as A, so the same version.
    write_workflow("a");
    let (reverted, version_reverted) = compile("reverted.json");
    assert_eq!(version_reverted, version_a);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &reverted]),
        "apply the revert",
    );
    assert!(!stdout.contains("nothing to do"), "{stdout}");
    assert_ok(
        &package_cmd(&["diff", "-s", &target.url(), "-f", &reverted]),
        "no drift after the revert",
    );
    assert_eq!(current().await, version_a);
}

/// #340: a target whose `[plugins.trust]` names keys refuses an unsigned
/// plugin; `--signatures <dir>` attaches the deployment's signatures at
/// plan and apply without touching the artifact, and a re-apply with a
/// rotated key's signatures re-signs the applied plugin.
#[tokio::test]
async fn package_apply_attaches_signatures_from_a_directory() {
    use orion::crypto::ed25519::SigningKey;
    let first = SigningKey::generate();
    let second = SigningKey::generate();
    let keys = format!(
        "{},{}",
        first.public_key_base64(),
        second.public_key_base64()
    );
    let client = reqwest::Client::new();
    let target = Server::start_with(
        "trust-target",
        &[("ORION_PLUGINS__TRUST__PUBLIC_KEYS", keys.as_str())],
    );
    target.wait_ready(&client).await;

    // A set with a plugin, compiled without signatures: the build does not
    // hold the deployment's key.
    let defs = ScratchDir::new("sig-defs");
    let dir = defs.path();
    std::fs::create_dir_all(dir.join("codec")).expect("dir");
    std::fs::write(
        dir.join("codec/plugin.toml"),
        include_str!("../fixtures/plugins/fixture-upload.toml"),
    )
    .expect("manifest");
    let component = include_bytes!("../fixtures/plugins/fixture.wasm");
    std::fs::write(dir.join("codec/fixture.wasm"), component).expect("component");
    std::fs::write(
        dir.join("wf.json"),
        serde_json::json!({
            "workflow_id": "wrap", "name": "Wrap",
            "tasks": [
                {"id": "parse", "name": "Parse", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "input"}}},
                {"id": "wrap", "name": "Wrap", "function": {"name": "test.fixture.wrap",
                    "input": {"message": {"var": "data.input.msg"}, "output": "data.result"}}}
            ],
        })
        .to_string(),
    )
    .expect("workflow");
    std::fs::write(
        dir.join("ch.json"),
        serde_json::json!({
            "channel_id": "wrap-api", "name": "wrap-api", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/wrap",
            "workflow_id": "wrap",
        })
        .to_string(),
    )
    .expect("channel");
    let out = ScratchDir::new("sig-out");
    let artifact = out.path().join("codec.json");
    let artifact = artifact.to_str().expect("utf8");
    let compiled = Command::new(orion_bin())
        .args([
            "compile",
            dir.to_str().expect("utf8"),
            "--name",
            "codec",
            "--version",
            "1.0.0",
            "-o",
            artifact,
        ])
        .output()
        .expect("compile");
    assert_ok(&compiled, "compile");
    let before = std::fs::read_to_string(artifact).expect("artifact");

    let digest = orion::crypto::sha256_digest(component);
    let sign_into = |key: &SigningKey, label: &str| {
        let sigs = ScratchDir::new(label);
        std::fs::write(
            sigs.path().join("fixture.wasm.sig"),
            format!("{}\n", key.sign(&digest)),
        )
        .expect("sig");
        sigs
    };
    let stored_signature = || async {
        let row: serde_json::Value = client
            .get(format!(
                "{}/api/v1/admin/plugins/test.fixture",
                target.url()
            ))
            .send()
            .await
            .expect("plugin")
            .json()
            .await
            .expect("json");
        row["data"]["signature"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    // Unsigned, the target refuses at plan.
    let out = package_cmd(&["plan", "-s", &target.url(), "-f", artifact]);
    assert!(!out.status.success(), "an unsigned plugin must be refused");

    let sigs = sign_into(&first, "sigs-first");
    let sigs_path = sigs.path().to_str().expect("utf8");
    let stdout = assert_ok(
        &package_cmd(&[
            "plan",
            "-s",
            &target.url(),
            "-f",
            artifact,
            "--signatures",
            sigs_path,
        ]),
        "plan with signatures",
    );
    assert!(stdout.contains("signed    test.fixture"), "{stdout}");
    let stdout = assert_ok(
        &package_cmd(&[
            "apply",
            "-s",
            &target.url(),
            "-f",
            artifact,
            "--signatures",
            sigs_path,
        ]),
        "apply with signatures",
    );
    assert!(stdout.contains("applied codec@1.0.0"), "{stdout}");
    assert_eq!(stored_signature().await, first.sign(&digest));
    assert_eq!(
        std::fs::read_to_string(artifact).expect("artifact"),
        before,
        "the artifact file is untouched"
    );
    let resp = client
        .post(format!("{}/api/v1/data/wrap", target.url()))
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

    // The same signatures again: nothing to do.
    let stdout = assert_ok(
        &package_cmd(&[
            "apply",
            "-s",
            &target.url(),
            "-f",
            artifact,
            "--signatures",
            sigs_path,
        ]),
        "re-apply",
    );
    assert!(stdout.contains("nothing to do"), "{stdout}");

    // A rotated key's signatures re-sign the applied plugin; the content,
    // and so the receipt, does not move.
    let rotated = sign_into(&second, "sigs-second");
    let stdout = assert_ok(
        &package_cmd(&[
            "apply",
            "-s",
            &target.url(),
            "-f",
            artifact,
            "--signatures",
            rotated.path().to_str().expect("utf8"),
        ]),
        "apply a rotated key",
    );
    assert!(
        stdout.contains("re-signed plugins 'test.fixture'"),
        "{stdout}"
    );
    assert_eq!(stored_signature().await, second.sign(&digest));
    let resp = client
        .post(format!("{}/api/v1/data/wrap", target.url()))
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
}

/// #342: "applied" must mean serving. A node restarted with the scheduler
/// off quarantines the cron channel an applied package carries: re-applying
/// the same version fails naming it (instead of "nothing to do"), and a new
/// version's apply fails after the reload with its receipt left `staged`.
/// Turn the scheduler back on and the same apply completes.
#[tokio::test]
async fn apply_fails_when_the_reload_quarantines_what_it_carries() {
    let client = reqwest::Client::new();
    let target = Server::start("cron-target");
    target.wait_ready(&client).await;

    let defs = ScratchDir::new("cron-defs");
    let dir = defs.path();
    let write_workflow = |message: &str| {
        std::fs::write(
            dir.join("wf.json"),
            serde_json::json!({
                "workflow_id": "nightly", "name": "Nightly",
                "tasks": [{"id": "t1", "name": "log",
                           "function": {"name": "log", "input": {"message": message}}}],
            })
            .to_string(),
        )
        .expect("workflow");
    };
    write_workflow("v1");
    std::fs::write(
        dir.join("ch.json"),
        serde_json::json!({
            "channel_id": "nightly-sweep", "name": "nightly-sweep", "channel_type": "async",
            "protocol": "cron", "workflow_id": "nightly",
            "transport_config": {"schedule": "0 15 2 * * *", "timezone": "UTC", "payload": {}},
        })
        .to_string(),
    )
    .expect("channel");
    let out = ScratchDir::new("cron-out");
    let compile = |version: &str| -> String {
        let path = out.path().join(format!("{version}.json"));
        let path = path.to_str().expect("utf8").to_string();
        let result = Command::new(orion_bin())
            .args([
                "compile",
                dir.to_str().expect("utf8"),
                "--name",
                "nightly",
                "--version",
                version,
                "-o",
                &path,
            ])
            .output()
            .expect("compile");
        assert_ok(&result, "compile");
        path
    };
    let v1 = compile("1.0.0");
    assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &v1]),
        "apply with the scheduler on",
    );

    // The same database, the scheduler off.
    let target = target.restart_with(&[("ORION_CRON__ENABLED", "false")]);
    target.wait_ready(&client).await;

    // (a) The applied version is not "nothing to do" any more.
    let out_a = package_cmd(&["apply", "-s", &target.url(), "-f", &v1]);
    let stderr = String::from_utf8_lossy(&out_a.stderr);
    assert!(!out_a.status.success(), "{stderr}");
    assert!(
        stderr.contains("channels/nightly-sweep") && stderr.contains("cron.enabled = false"),
        "{stderr}"
    );
    assert!(stderr.contains("already applied and stays so"), "{stderr}");

    // plan predicts it.
    write_workflow("v2");
    let v2 = compile("2.0.0");
    let planned = package_cmd(&["plan", "-s", &target.url(), "-f", &v2]);
    let stderr = String::from_utf8_lossy(&planned.stderr);
    assert!(
        stderr.contains("target has cron.enabled = false")
            || stderr.contains("already quarantined on the target"),
        "{stderr}"
    );

    // (b) A new version fails — on the node that answers, the channel's
    // activation gate refuses it before the reload — and its receipt stays
    // staged.
    let out_b = package_cmd(&["apply", "-s", &target.url(), "-f", &v2]);
    let stderr = String::from_utf8_lossy(&out_b.stderr);
    assert!(!out_b.status.success(), "{stderr}");
    assert!(
        stderr.contains("nightly-sweep") && stderr.contains("cron.enabled = false"),
        "{stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("the receipt stays staged"),
        "{stderr}"
    );
    let receipts: serde_json::Value = client
        .get(format!("{}/api/v1/admin/packages/nightly", target.url()))
        .send()
        .await
        .expect("receipts")
        .json()
        .await
        .expect("json");
    let v2_state = receipts["data"]["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .find(|r| r["version"] == "2.0.0")
        .map(|r| r["state"].clone());
    assert_eq!(v2_state, Some(serde_json::json!("staged")), "{receipts}");
    assert_eq!(receipts["data"]["current"]["version"], "1.0.0");

    // (c) With the scheduler back on, the same apply completes.
    let target = target.restart_with(&[]);
    target.wait_ready(&client).await;
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &v2]),
        "apply with the scheduler back on",
    );
    assert!(stdout.contains("applied nightly@2.0.0"), "{stdout}");
}

/// #342, phase 4b: a member nothing refuses at activation — a connector,
/// which has none — can still fail to load at the reload. Apply reads the
/// reload's own answer, fails naming it, and leaves the receipt staged.
#[tokio::test]
async fn apply_fails_when_a_carried_connector_does_not_load() {
    let client = reqwest::Client::new();
    let target = Server::start("connector-target");
    target.wait_ready(&client).await;

    let defs = ScratchDir::new("connector-defs");
    let dir = defs.path();
    std::fs::write(
        dir.join("conn.json"),
        serde_json::json!({
            "name": "crm", "connector_type": "http",
            "config": {"url": "https://crm.example.com",
                       "auth": {"type": "bearer", "token": "env://ORION_T342_NEVER_SET"}},
        })
        .to_string(),
    )
    .expect("connector");
    std::fs::write(
        dir.join("wf.json"),
        serde_json::json!({
            "workflow_id": "crm-flow", "name": "CRM",
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": "hi"}}}],
        })
        .to_string(),
    )
    .expect("workflow");
    let out = ScratchDir::new("connector-out");
    let artifact = out.path().join("crm.json");
    let artifact = artifact.to_str().expect("utf8");
    let compiled = Command::new(orion_bin())
        .args([
            "compile",
            dir.to_str().expect("utf8"),
            "--name",
            "crm",
            "--version",
            "1.0.0",
            "-o",
            artifact,
        ])
        .output()
        .expect("compile");
    assert_ok(&compiled, "compile");

    let applied = package_cmd(&["apply", "-s", &target.url(), "-f", artifact]);
    let stderr = String::from_utf8_lossy(&applied.stderr);
    assert!(!applied.status.success(), "{stderr}");
    assert!(
        stderr.contains("connectors/crm: secret_resolution")
            && stderr.contains("ORION_T342_NEVER_SET"),
        "{stderr}"
    );
    assert!(stderr.contains("the receipt stays staged"), "{stderr}");
    let receipts: serde_json::Value = client
        .get(format!("{}/api/v1/admin/packages/crm", target.url()))
        .send()
        .await
        .expect("receipts")
        .json()
        .await
        .expect("json");
    assert_eq!(
        receipts["data"]["versions"][0]["state"], "staged",
        "{receipts}"
    );
    assert!(receipts["data"]["current"].is_null(), "{receipts}");
}

/// #343: an artifact's `requires.orion` holds the target to a range —
/// `plan` and `apply` refuse one outside it, naming both, before anything is
/// written; a satisfied range applies as before.
#[tokio::test]
async fn plan_and_apply_refuse_a_target_outside_the_declared_range() {
    let client = reqwest::Client::new();
    let target = Server::start("range-target");
    target.wait_ready(&client).await;

    let defs = ScratchDir::new("range-defs");
    std::fs::write(
        defs.path().join("wf.json"),
        serde_json::json!({
            "workflow_id": "ranged", "name": "Ranged",
            "tasks": [{"id": "t1", "name": "log",
                       "function": {"name": "log", "input": {"message": "hi"}}}],
        })
        .to_string(),
    )
    .expect("workflow");
    let out = ScratchDir::new("range-out");
    let compile = |file: &str, range: &str| -> String {
        let path = out.path().join(file);
        let path = path.to_str().expect("utf8").to_string();
        let result = Command::new(orion_bin())
            .args([
                "compile",
                defs.path().to_str().expect("utf8"),
                "--name",
                "ranged",
                "--version",
                "1.0.0",
                "--requires-orion",
                range,
                "-o",
                &path,
            ])
            .output()
            .expect("compile");
        assert_ok(&result, "compile");
        path
    };

    let too_new = compile("too-new.json", ">=99.0.0");
    for verb in ["plan", "apply"] {
        let result = package_cmd(&[verb, "-s", &target.url(), "-f", &too_new]);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{verb}: {stderr}");
        assert!(
            stderr.contains("ranged@1.0.0 requires Orion >=99.0.0; the target")
                && stderr.contains(&format!("runs {}", env!("CARGO_PKG_VERSION"))),
            "{verb}: {stderr}"
        );
    }
    let receipt = client
        .get(format!("{}/api/v1/admin/packages/ranged", target.url()))
        .send()
        .await
        .expect("receipt");
    assert_eq!(receipt.status(), 404, "no receipt may have been claimed");

    let satisfied = compile(
        "satisfied.json",
        &format!(">={}", env!("CARGO_PKG_VERSION")),
    );
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &target.url(), "-f", &satisfied]),
        "apply within range",
    );
    assert!(stdout.contains("applied ranged@1.0.0"), "{stdout}");
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

/// A definition set on disk, compiled at an explicit version: the shape a
/// deploy pipeline feeds `apply` from.
struct DefinitionSet {
    defs: ScratchDir,
    out: ScratchDir,
    name: &'static str,
}

impl DefinitionSet {
    fn new(name: &'static str) -> Self {
        Self {
            defs: ScratchDir::new("prune-defs"),
            out: ScratchDir::new("prune-artifacts"),
            name,
        }
    }

    fn write(&self, file: &str, value: serde_json::Value) {
        std::fs::write(self.defs.path().join(file), value.to_string()).expect("write definition");
    }

    fn remove(&self, file: &str) {
        std::fs::remove_file(self.defs.path().join(file)).expect("remove definition");
    }

    fn workflow(&self, id: &str) {
        self.write(
            &format!("wf-{id}.json"),
            serde_json::json!({
                "workflow_id": id, "name": id,
                "tasks": [{"id": "t1", "name": "log",
                           "function": {"name": "log", "input": {"message": id}}}],
            }),
        );
    }

    fn channel(&self, id: &str, route: &str, workflow: &str) {
        self.write(
            &format!("ch-{id}.json"),
            serde_json::json!({
                "channel_id": id, "name": id, "channel_type": "sync", "protocol": "rest",
                "methods": ["POST"], "route_pattern": route, "workflow_id": workflow,
            }),
        );
    }

    fn connector(&self, name: &str) {
        self.write(
            &format!("conn-{name}.json"),
            serde_json::json!({
                "name": name, "connector_type": "http",
                "config": {"type": "http", "url": "https://example.com"},
            }),
        );
    }

    /// Compile the set as `version` and return the artifact's path.
    fn compile(&self, version: &str) -> String {
        let path = self.out.path().join(format!("{version}.json"));
        let path = path.to_str().expect("utf8").to_string();
        let result = Command::new(orion_bin())
            .args([
                "compile",
                self.defs.path().to_str().expect("utf8"),
                "--name",
                self.name,
                "--version",
                version,
                "-o",
                &path,
            ])
            .output()
            .expect("compile");
        assert_ok(&result, "compile");
        path
    }
}

async fn get_json(client: &reqwest::Client, url: String) -> (u16, serde_json::Value) {
    let resp = client.get(url).send().await.expect("GET");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or_default())
}

#[track_caller]
fn assert_fails(out: &std::process::Output, what: &str) -> (String, String) {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "{what} should have failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

/// #341: `--prune` removes what the previous applied version carried and
/// this one does not — a dropped channel archived before activation, so its
/// route can move to a new channel id in the same apply, and a dropped
/// connector disabled — and `--prune=delete` deletes.
#[tokio::test]
async fn prune_removes_what_the_previous_version_held() {
    let client = reqwest::Client::new();
    let target = Server::start("target");
    target.wait_ready(&client).await;
    let base = target.url();

    let set = DefinitionSet::new("orders");
    set.workflow("wf-a");
    set.workflow("wf-b");
    set.channel("ch-a", "/prune-a", "wf-a");
    set.channel("ch-old", "/prune-moved", "wf-b");
    set.connector("spare");
    let v1 = set.compile("1.0.0");
    assert_ok(&package_cmd(&["apply", "-s", &base, "-f", &v1]), "apply v1");
    let (_, receipt) = get_json(&client, format!("{base}/api/v1/admin/packages/orders")).await;
    assert_eq!(
        receipt["data"]["current"]["inventory"]["channels"],
        serde_json::json!(["ch-a", "ch-old"]),
        "{receipt}"
    );

    // v2 moves ch-old's route to a new channel id and drops the connector.
    set.remove("ch-ch-old.json");
    set.channel("ch-new", "/prune-moved", "wf-b");
    set.remove("conn-spare.json");
    let v2 = set.compile("1.1.0");

    let stdout = assert_ok(&package_cmd(&["plan", "-s", &base, "-f", &v2]), "plan v2");
    assert!(
        stdout.contains("note: 2 entities of orders@1.0.0 are not in this artifact"),
        "{stdout}"
    );
    let stdout = assert_ok(
        &package_cmd(&["plan", "-s", &base, "-f", &v2, "--prune"]),
        "plan v2 --prune",
    );
    assert!(
        stdout.contains("prune: archive (in orders@1.0.0, not in this artifact)"),
        "{stdout}"
    );
    assert!(stdout.contains("prune: disable"), "{stdout}");
    assert!(
        stdout.contains("gate pending apply order"),
        "the route collision with ch-old is resolved by the prune: {stdout}"
    );

    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &v2, "--prune"]),
        "apply v2 --prune",
    );
    assert!(
        stdout.contains("pruned channels 'ch-old' (archived)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("pruned connectors 'spare' (disabled)"),
        "{stdout}"
    );
    let (_, old) = get_json(&client, format!("{base}/api/v1/admin/channels/ch-old")).await;
    assert_eq!(old["data"]["status"], "archived", "{old}");
    let (_, connectors) = get_json(&client, format!("{base}/api/v1/admin/connectors")).await;
    let spare = connectors["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "spare")
        .expect("the connector is disabled, not deleted");
    assert_eq!(spare["enabled"], false, "{spare}");
    // The route is served by the new channel.
    let resp = client
        .post(format!("{base}/api/v1/data/prune-moved"))
        .json(&serde_json::json!({"data": {}}))
        .send()
        .await
        .expect("data call");
    assert_eq!(resp.status(), 200);
    let (_, status) = get_json(&client, format!("{base}/api/v1/admin/engine/status")).await;
    assert!(
        status["data"]["load_issues"]["channels"]
            .as_array()
            .is_none_or(|c| c.is_empty()),
        "{status}"
    );

    // Re-running the same deploy prunes nothing.
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &v2, "--prune"]),
        "re-apply v2 --prune",
    );
    assert!(stdout.contains("nothing to prune"), "{stdout}");

    // v3 drops wf-b and its channel, deleted this time.
    set.remove("wf-wf-b.json");
    set.remove("ch-ch-new.json");
    let v3 = set.compile("1.2.0");
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &v3, "--prune=delete"]),
        "apply v3 --prune=delete",
    );
    assert!(
        stdout.contains("pruned channels 'ch-new' (deleted)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("pruned workflows 'wf-b' (deleted)"),
        "{stdout}"
    );
    let (code, _) = get_json(&client, format!("{base}/api/v1/admin/workflows/wf-b")).await;
    assert_eq!(code, 404);
    let (code, _) = get_json(&client, format!("{base}/api/v1/admin/channels/ch-new")).await;
    assert_eq!(code, 404);
}

/// #341: a removal something outside the prune depends on is refused with
/// nothing written; an entity another package now carries is kept; and a
/// receipt from before inventories prunes nothing and says so.
#[tokio::test]
async fn prune_refuses_keeps_and_explains() {
    let client = reqwest::Client::new();
    let target = Server::start("target");
    target.wait_ready(&client).await;
    let base = target.url();

    let set = DefinitionSet::new("orders");
    set.workflow("wf-x");
    set.workflow("wf-y");
    set.channel("ch-x", "/refuse-x", "wf-x");
    set.connector("shared");
    let v1 = set.compile("1.0.0");
    assert_ok(&package_cmd(&["apply", "-s", &base, "-f", &v1]), "apply v1");

    // Another package takes over the connector.
    let billing = DefinitionSet::new("billing");
    billing.connector("shared");
    let billing_v1 = billing.compile("2.1.0");
    assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &billing_v1]),
        "apply billing",
    );

    // A channel outside any package routes to wf-y.
    let resp = client
        .post(format!("{base}/api/v1/admin/channels"))
        .json(&serde_json::json!({
            "channel_id": "outside", "name": "outside", "channel_type": "sync",
            "protocol": "rest", "methods": ["POST"], "route_pattern": "/outside",
            "workflow_id": "wf-y",
        }))
        .send()
        .await
        .expect("create outside channel");
    assert_eq!(resp.status(), 201);
    let resp = client
        .patch(format!("{base}/api/v1/admin/channels/outside/status"))
        .json(&serde_json::json!({"status": "active"}))
        .send()
        .await
        .expect("activate outside channel");
    assert_eq!(resp.status(), 200);

    set.remove("wf-wf-y.json");
    set.remove("conn-shared.json");
    let v2 = set.compile("1.1.0");
    let (_, stderr) = assert_fails(
        &package_cmd(&["plan", "-s", &base, "-f", &v2, "--prune"]),
        "plan v2 --prune",
    );
    assert!(
        stderr.contains(
            "cannot prune workflows/wf-y: active channel 'outside' (not in this package) still \
             routes to it"
        ),
        "{stderr}"
    );
    let (stdout, stderr) = assert_fails(
        &package_cmd(&["apply", "-s", &base, "-f", &v2, "--prune"]),
        "apply v2 --prune",
    );
    assert!(stderr.contains("nothing was written"), "{stderr}");
    assert!(
        stdout.contains("keep: now carried by billing@2.1.0"),
        "{stdout}"
    );
    let (_, receipt) = get_json(&client, format!("{base}/api/v1/admin/packages/orders")).await;
    assert_eq!(
        receipt["data"]["versions"].as_array().map(Vec::len),
        Some(1),
        "the refused apply claimed no receipt: {receipt}"
    );
    let (_, wf) = get_json(&client, format!("{base}/api/v1/admin/workflows/wf-y")).await;
    assert_eq!(wf["data"]["status"], "active", "{wf}");

    // Once the outside channel is gone, the prune goes through and keeps
    // the connector billing carries.
    let resp = client
        .delete(format!("{base}/api/v1/admin/channels/outside"))
        .send()
        .await
        .expect("delete outside channel");
    assert_eq!(resp.status(), 204);
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &v2, "--prune"]),
        "apply v2 --prune",
    );
    assert!(
        stdout.contains("pruned workflows 'wf-y' (archived)"),
        "{stdout}"
    );
    assert!(!stdout.contains("pruned connectors"), "{stdout}");

    // A receipt recorded before inventories: nothing to measure from.
    let resp = client
        .put(format!("{base}/api/v1/admin/packages/legacy"))
        .json(&serde_json::json!({
            "version": "0.9.0", "content_hash": "sha256:old", "state": "applied",
        }))
        .send()
        .await
        .expect("legacy receipt");
    assert_eq!(resp.status(), 200);
    let legacy = DefinitionSet::new("legacy");
    legacy.workflow("wf-legacy");
    let legacy_v1 = legacy.compile("1.0.0");
    let stdout = assert_ok(
        &package_cmd(&["apply", "-s", &base, "-f", &legacy_v1, "--prune"]),
        "apply legacy --prune",
    );
    assert!(
        stdout.contains("legacy@0.9.0 was applied before receipts recorded what they carried"),
        "{stdout}"
    );
}
