//! Secrets and vars in the offline tools: `dry-run --secrets`, a case
//! file's `secrets`/`metadata.vars`, and `lint`'s secret inventory. A node's
//! own handling of `[secrets]`/`[vars]` is tested in the server crate.

use crate::common::{ScratchDir, orion_bin};
use serde_json::json;

fn signing_workflow_json() -> &'static str {
    r#"{
        "name": "offline signer",
        "condition": true,
        "tasks": [{
            "id": "sign", "name": "Sign",
            "function": {"name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256",
                "key": {"secret": "partner_hmac"},
                "data": "order-4711",
                "output": "data.mac"
            }}
        }]
    }"#
}

/// A one-task workflow signing `data` with `key`.
fn hmac_workflow(name: &str, key: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [{
            "id": "sign", "name": "Sign",
            "function": { "name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256",
                "key": key,
                "data": "order-4711",
                "output": "data.mac"
            } }
        }]
    })
}

fn write_scratch(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write fixture");
    path.to_string_lossy().into_owned()
}

/// `dry-run --secrets` supplies the stand-ins an offline run has no config to
/// resolve. Without it the engine has no store and refuses the workflow, so a
/// workflow that signs anything would be untestable.
#[test]
fn dry_run_takes_stand_in_secrets() {
    let scratch = ScratchDir::new("dry-run-secrets");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);
    let secrets = write_scratch(dir, "secrets.json", r#"{"partner_hmac":"stand-in-key"}"#);

    let out = std::process::Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--secrets", &secrets])
        .output()
        .expect("invoke dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("\"mac\""),
        "the signing task must have run: {stdout}"
    );
    assert!(
        !stdout.contains("stand-in-key"),
        "even a stand-in must not be echoed into the printed run: {stdout}"
    );
}

/// And without the flag the run refuses rather than signing with `null` — the
/// same "an engine with no store refuses a workflow that names one" rule the
/// serving path applies.
#[test]
fn dry_run_without_secrets_refuses_a_workflow_that_names_one() {
    let scratch = ScratchDir::new("dry-run-no-secrets");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);

    let out = std::process::Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("invoke dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an undeclared secret must not run offline either: stdout={stdout}"
    );
    assert!(
        stderr.contains("partner_hmac"),
        "the refusal must name the key: stderr={stderr}"
    );
}

/// A `*.case.json` carries the same block, so a regression suite can cover a
/// workflow that signs. The case also pins `metadata.vars`, which offline is
/// supplied rather than stamped — there is no config file to stamp it from.
#[test]
fn a_case_file_takes_secrets_and_vars() {
    let scratch = ScratchDir::new("case-secrets");
    let dir = scratch.path();
    write_scratch(
        dir,
        "wf.json",
        r#"{
            "name": "offline signer",
            "condition": true,
            "tasks": [
                {
                    "id": "sign", "name": "Sign",
                    "function": {"name": "crypto", "input": {
                        "op": "hmac", "algorithm": "sha256",
                        "key": {"secret": "partner_hmac"},
                        "data": "order-4711",
                        "output": "data.mac"
                    }}
                },
                {
                    "id": "topic", "name": "Topic",
                    "function": {"name": "map", "input": {"mappings": [
                        {"path": "data.topic", "logic": {"var": "metadata.vars.topic_prefix"}}
                    ]}}
                }
            ]
        }"#,
    );
    write_scratch(
        dir,
        "ok.case.json",
        r#"{
            "name": "signs and reads a var",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "secrets": {"partner_hmac": "stand-in-key"},
            "metadata": {"vars": {"topic_prefix": "eu-west"}},
            "expect": {"data.topic": "eu-west"}
        }"#,
    );

    let out = std::process::Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "suite failed: {stdout}");
    assert!(stdout.contains("1 passed, 0 failed"), "{stdout}");
    assert!(
        !stdout.contains("stand-in-key"),
        "the runner must not echo a stand-in value: {stdout}"
    );
}

/// The same node, seen by `lint`'s deployment inventory. A set that names a
/// secret the target instance must declare has to be listed whichever spelling
/// it used, or the checklist is short by exactly the entry that quarantines a
/// channel on deploy.
#[test]
fn lint_inventories_both_spellings_of_a_secret_reference() {
    for spelling in [
        json!({"secret": "partner_hmac"}),
        json!({"secret": ["partner_hmac"]}),
    ] {
        let scratch = ScratchDir::new("lint-secret-spelling");
        write_scratch(
            scratch.path(),
            "wf.json",
            &hmac_workflow("lint wf", spelling.clone()).to_string(),
        );
        let out = std::process::Command::new(orion_bin())
            .args(["lint", scratch.path().to_str().expect("utf-8 path")])
            .output()
            .expect("invoke lint");
        // The inventory is a note, and notes go to stderr; the summary line is
        // what lands on stdout.
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            rendered.contains("[secrets.reference]") && rendered.contains("partner_hmac"),
            "{spelling} must appear in the [secrets] inventory: {rendered}"
        );
    }
}

/// A `--secrets` / case-file store is the only one that can hold a non-string,
/// so its shape is checked where the file is named rather than at the first
/// task that reads one — a case-file run reports a task error under the case
/// name and nothing else.
#[test]
fn an_offline_secret_must_be_a_string() {
    let scratch = ScratchDir::new("offline-secret-kind");
    let dir = scratch.path();
    let wf = write_scratch(dir, "wf.json", signing_workflow_json());
    let input = write_scratch(dir, "input.json", r#"{"id":"ORD-1"}"#);
    let secrets = write_scratch(dir, "secrets.json", r#"{"partner_hmac": 12345}"#);

    let out = std::process::Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--secrets", &secrets])
        .output()
        .expect("invoke dry-run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "a non-string stand-in must fail at load: {combined}"
    );
    assert!(
        combined.contains("partner_hmac") && combined.contains("string"),
        "the failure must name the entry and the kind it needed: {combined}"
    );
}
