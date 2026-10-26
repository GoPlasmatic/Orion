//! `orion-cli compile --format deployment` → `orion-cli deployments
//! apply -d`, end to end: both binaries, a real server, the wire between.
//!
//! An example set compiles to a deployment directory and deploys; the
//! channel answers. One workflow changes, the set recompiles and re-applies,
//! and only that member's revision moves. Applying an unchanged directory
//! applies nothing.
//!
//! The server binary is the one `cargo test --workspace` builds beside this
//! crate's (`target/<profile>/orion-server`), or `ORION_BIN`. Run alone with
//! `cargo test -p orion-cli` and no server binary built, the test says so
//! and passes without running.

#![expect(clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

use serde_json::Value;

fn orion_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orion-cli"))
}

fn orion_server() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ORION_BIN") {
        return Some(PathBuf::from(path));
    }
    let beside =
        orion_cli().with_file_name(format!("orion-server{}", std::env::consts::EXE_SUFFIX));
    beside.is_file().then_some(beside)
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A server on its own database, killed on drop.
struct Server {
    child: Child,
    url: String,
    log: PathBuf,
}

impl Server {
    fn start(bin: &Path, dir: &Path) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let log = dir.join("server.log");
        let out = std::fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = Command::new(bin)
            .env(
                "ORION_STORAGE__URL",
                format!("sqlite:{}?mode=rwc", dir.join("orion.db").display()),
            )
            .env("ORION_SERVER__PORT", port.to_string())
            .env("ORION_LOGGING__LEVEL", "warn")
            .stdout(out)
            .stderr(err)
            .spawn()
            .unwrap();
        Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            log,
        }
    }

    async fn wait_ready(&self) {
        let client = orion_client::OrionClient::new(&self.url).unwrap();
        for _ in 0..120 {
            if client.get_text("/readyz").await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!(
            "the server never became ready:\n{}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(orion_cli())
            .args(["--server", &self.url, "--output", "json", "--no-color"])
            .args(args)
            .output()
            .unwrap()
    }

    /// Run the CLI, requiring success, and parse its stdout as JSON.
    fn cli_json(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "orion-cli {args:?} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "orion-cli {args:?}: stdout is not JSON ({e}):\n{}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "--- server log ---\n{}",
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}

fn compile(set: &Path, out: &Path) {
    let result = Command::new(orion_cli())
        .args([
            "compile",
            set.to_str().unwrap(),
            "--format",
            "deployment",
            "-o",
        ])
        .arg(out)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "compile failed:\n{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// `(kind, id) -> revision` of what deployment `revision` deploys.
fn member_revisions(server: &Server, revision: u64) -> Vec<(String, String, u64)> {
    let got = server.cli_json(&["deployments", "get", &revision.to_string()]);
    let mut members: Vec<(String, String, u64)> = got["manifest"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["entity"]["kind"].as_str().unwrap().to_string(),
                m["entity"]["id"].as_str().unwrap().to_string(),
                m["entity"]["revision"].as_u64().unwrap(),
            )
        })
        .collect();
    members.sort();
    members
}

#[tokio::test]
async fn a_compiled_deployment_deploys_and_redeploys_only_what_changed() {
    let Some(server_bin) = orion_server() else {
        eprintln!(
            "skipped: no orion-server beside {} — `cargo test --workspace` builds one, or set \
             ORION_BIN",
            orion_cli().display()
        );
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let set = scratch.path().join("set");
    std::fs::create_dir_all(&set).unwrap();
    let example = workspace().join("examples/packages/order-classification");
    for file in ["workflow.json", "channel.json"] {
        std::fs::copy(example.join(file), set.join(file)).unwrap();
    }
    // A connection rides along, so its revision is assigned and kept too.
    std::fs::write(
        set.join("connector.json"),
        r#"{ "name": "scratch-cache", "connector_type": "cache",
             "config": { "type": "cache", "backend": "memory" } }"#,
    )
    .unwrap();
    let dist = scratch.path().join("dist");
    compile(&set, &dist);

    let server = Server::start(&server_bin, scratch.path());
    server.wait_ready().await;

    let dir = dist.to_str().unwrap();
    let applied = server.cli_json(&["deployments", "apply", "-d", dir]);
    assert_eq!(applied["committed_revision"], 1, "{applied}");
    assert_eq!(
        member_revisions(&server, 1),
        [
            ("channel".into(), "order-tiers".into(), 1),
            ("connection".into(), "scratch-cache".into(), 1),
            ("workflow".into(), "order-classification".into(), 1),
        ]
    );

    let answer = server.cli_json(&["send", "order-tiers", "-d", r#"{"amount": 750}"#]);
    assert_eq!(answer["data"]["order"]["tier"], "vip", "{answer}");
    assert_eq!(answer["data"]["order"]["discount_pct"], 15, "{answer}");

    // One workflow changes; only its member moves.
    let workflow = std::fs::read_to_string(set.join("workflow.json"))
        .unwrap()
        .replace(
            r#"{ "path": "data.order.discount_pct", "logic": 15 }"#,
            r#"{ "path": "data.order.discount_pct", "logic": 20 }"#,
        );
    assert!(workflow.contains("\"logic\": 20"), "the edit applies");
    std::fs::write(set.join("workflow.json"), workflow).unwrap();
    compile(&set, &dist);

    let applied = server.cli_json(&["deployments", "apply", "-d", dir]);
    assert_eq!(applied["committed_revision"], 2, "{applied}");
    assert_eq!(
        member_revisions(&server, 2),
        [
            ("channel".into(), "order-tiers".into(), 1),
            ("connection".into(), "scratch-cache".into(), 1),
            ("workflow".into(), "order-classification".into(), 2),
        ]
    );
    let answer = server.cli_json(&["send", "order-tiers", "-d", r#"{"amount": 750}"#]);
    assert_eq!(answer["data"]["order"]["discount_pct"], 20, "{answer}");

    // Unchanged: nothing is applied, and the head stays where it is.
    let out = server.cli(&["deployments", "apply", "-d", dir]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("deploys this already"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = server.cli_json(&["deployments", "get", "2"]);
    assert_eq!(summary["revision"], 2);
    let out = server.cli(&["deployments", "get", "3"]);
    assert!(!out.status.success(), "no revision 3 was committed");
}
