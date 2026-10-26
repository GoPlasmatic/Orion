//! A6 (Tier-1 ergonomics): offline CLI subcommands `lint`, `dry-run` and `test`.
//!
//! Snapshot-style tests against the compiled binary. The binary path is
//! resolved from cargo's CARGO_BIN_EXE_<name> env var so these run on
//! whatever target was just built.

use std::process::Command;

use crate::common::{ScratchDir, orion_bin};

fn write_temp(content: &str, suffix: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "orion-cli-test-{}-{}.json",
        suffix,
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

/// A workflow whose second task calls a connector, so it cannot run offline
/// without a stub.
const CONNECTOR_WORKFLOW: &str = r#"{
    "name": "enrich",
    "condition": true,
    "tasks": [
        {"id":"parse","name":"Parse","function":{
            "name":"parse_json","input":{"source":"payload","target":"order"}}},
        {"id":"lookup","name":"Lookup","function":{
            "name":"http_call","input":{
                "connector":"crm","method":"GET","path":"/c/1","output":"data.customer"}}},
        {"id":"shape","name":"Shape","function":{
            "name":"map","input":{"mappings":[
                {"path":"data.order.customer_name","logic":{"var":"data.customer.name"}}
            ]}}}
    ]
}"#;

/// A directory holding one workflow plus whatever case files a test writes.
fn temp_suite() -> ScratchDir {
    suite_with(CONNECTOR_WORKFLOW)
}

/// A workflow that branches on a request header and writes what it decided.
///
/// The offline gap this closes: without case metadata the header is always
/// absent, so exactly one branch is reachable and the others can only be tested
/// by curling a running server.
const HEADER_BRANCH_WORKFLOW: &str = r#"{
    "name": "login-mode",
    "condition": true,
    "tasks": [
        {"id":"mode","name":"Pick mode","function":{
            "name":"map","input":{"mappings":[
                {"path":"data.mode","logic":{"if":[
                    {"var":"metadata.headers.deviceid"}, "device", "password"]}},
                {"path":"data.device","logic":{"var":"metadata.headers.deviceid"}},
                {"path":"data.subject","logic":{"var":"metadata.auth.claims.sub"}},
                {"path":"data.page","logic":{"var":"metadata.query.page"}},
                {"path":"data.token","logic":{"var":"metadata.headers.authorization"}}
            ]}}}
    ]
}"#;

/// A workflow whose write payload carries an unresolvable JSONLogic node —
/// the bug #283 reports, which a stubbed run cannot currently see.
const VERBATIM_LOGIC_WORKFLOW: &str = r#"{
    "name": "rotate",
    "condition": true,
    "tasks": [
        {"id":"seed","name":"Seed","function":{
            "name":"map","input":{"mappings":[
                {"path":"temp_data.sid","logic":"sess-1"},
                {"path":"data.done","logic":true}
            ]}}},
        {"id":"persist","name":"Persist","function":{
            "name":"cache_write","input":{
                "connector":"sessions","key":"session",
                "value":{"_id":{"var":"temp_data.sid"},
                         "generation":{"if":[true,2,1]},"revokedAt":null}}}}
    ]
}"#;

/// [`temp_suite`] over an explicit workflow. Cleanup is the `ScratchDir`
/// drop, so a failing assertion does not leave the directory behind.
fn suite_with(workflow: &str) -> ScratchDir {
    let dir = ScratchDir::new("suite");
    std::fs::write(dir.path().join("wf.json"), workflow).unwrap();
    dir
}

fn run_suite(dir: &std::path::Path) -> (bool, String) {
    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// A directory holding whatever files a test writes.
fn temp_defs() -> ScratchDir {
    ScratchDir::new("defs")
}

fn lint_dir(dir: &std::path::Path, extra: &[&str]) -> (bool, String) {
    let mut args = vec!["lint", dir.to_str().unwrap()];
    args.extend_from_slice(extra);
    let out = Command::new(orion_bin())
        .args(&args)
        .output()
        .expect("run lint");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// A definitions directory holding a value catalog and one fragment.
fn temp_defs_with_shared() -> ScratchDir {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("fragments")).unwrap();
    std::fs::write(
        dir.join("common.json"),
        r#"{ "constants": { "db": { "connector": "pg", "schema": { "unmapped": "identity" } } },
             "errors": { "USER_NOT_FOUND": {
                "status": 400, "code": "NOT_FOUND", "body": "User Not Found !" } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("fragments/guard.json"),
        r#"{ "fragments": { "deny": {
              "params": { "message": { "default": "Denied." } },
              "tasks": [ { "id": "write", "name": "Write the refusal",
                "function": { "name": "map", "input": { "mappings": [
                  { "path": "data.denied", "logic": { "$param": "message" } } ] } } } ] } } }"#,
    )
    .unwrap();
    scratch
}

/// Writes a definition set of one workflow plus the given channel documents.
fn defs_with_channels(channels: &[&str]) -> ScratchDir {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[{"id":"t1","name":"t1","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    for (i, ch) in channels.iter().enumerate() {
        std::fs::write(dir.join(format!("ch{i}.json")), ch).unwrap();
    }
    scratch
}

fn lint_with_definitions(dir: &std::path::Path) -> (bool, String) {
    let out = Command::new(orion_bin())
        .args([
            "lint",
            dir.join("wf.json").to_str().unwrap(),
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run lint");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

const PLUGIN_MANIFEST: &str =
    include_str!("../../../../orion-server/tests/fixtures/plugins/fixture-upload.toml");
const PLUGIN_COMPONENT: &[u8] =
    include_bytes!("../../../../orion-server/tests/fixtures/plugins/fixture.wasm");

/// A directory holding the fixture plugin's manifest and, when asked, its
/// component — what `--plugin-dir` points at.
fn plugin_dir(label: &str, with_component: bool) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    std::fs::write(scratch.path().join("plugin.toml"), PLUGIN_MANIFEST).unwrap();
    if with_component {
        std::fs::write(scratch.path().join("fixture.wasm"), PLUGIN_COMPONENT).unwrap();
    }
    scratch
}

fn write_workflow(dir: &std::path::Path, name: &str, function: &str, input: &str) -> String {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            r#"{{"workflow_id":"w","name":"w","tasks":[
              {{"id":"parse","name":"parse","function":{{"name":"parse_json","input":{{"source":"payload","target":"input"}}}}}},
              {{"id":"t","name":"t","function":{{"name":"{function}","input":{input}}}}}]}}"#
        ),
    )
    .unwrap();
    path.to_str().unwrap().to_string()
}

fn run_cli(args: &[&str]) -> (bool, String) {
    let out = Command::new(orion_bin())
        .args(args)
        .output()
        .expect("run orion-cli");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// A set that declares `range`, with a workflow carrying a schema error the
/// version line must stand in front of (and a case file for `test`).
fn versioned_set(range: &str) -> ScratchDir {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("package.json"),
        serde_json::json!({"package": {"name": "orders", "requires": {"orion": range}}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"wf","name":"wf","tasks":[
             {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[]}}},
             {"id":"broken","name":"broken","function":{"name":"no_such_function_x","input":{}}}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("in.json"), r#"{"data": {}}"#).unwrap();
    std::fs::write(
        dir.join("wf.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {}}"#,
    )
    .unwrap();
    scratch
}

fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(orion_bin())
        .args(args)
        .output()
        .expect("run orion-cli");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A loop that rewrites a board every sweep: under the default execution
/// trace each step snapshotted the whole audit trail so far, so this shape
/// cost memory quadratic in the sweeps.
const LONG_LOOP_WORKFLOW: &str = r#"{
    "name": "long-loop",
    "loop": {"counter": "turn", "max": 3000},
    "tasks": [
        {"id":"board","name":"Board","function":{"name":"map","input":{"mappings":[
            {"path":"data.board","logic":{"var":"temp_data.turn"}}]}}},
        {"id":"count","name":"Count","function":{"name":"map","input":{"mappings":[
            {"path":"data.turns","logic":{"+":[{"var":"data.turns"},1]}}]}}}
    ]
}"#;

fn dry_run_json(workflow: &str, extra: &[&str]) -> serde_json::Value {
    let scratch = suite_with(workflow);
    let input = scratch.path().join("in.json");
    std::fs::write(&input, "{}").unwrap();
    let wf = scratch.path().join("wf.json");
    let mut args = vec![
        "dry-run",
        "-w",
        wf.to_str().unwrap(),
        "-i",
        input.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let out = Command::new(orion_bin())
        .args(&args)
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

mod dry_run_stubs;
mod lint_and_dry_run;
mod lint_set;
mod offline_cost;
mod plugins_offline;
mod shared_definitions;
mod sql_files;
mod task_groups;
mod test_runner;
mod version_requirement;
