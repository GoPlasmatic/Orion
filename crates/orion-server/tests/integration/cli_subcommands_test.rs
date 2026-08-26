//! A6 (Tier-1 ergonomics): CLI subcommands `lint`, `dry-run`, `test-connectivity`.
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

#[test]
fn lint_valid_workflow_exits_zero() {
    let wf = write_temp(
        r#"{
        "name": "smoke",
        "tasks": [
            {"id":"t1","name":"log","function":{"name":"log","input":{"message":"ok"}}}
        ]
    }"#,
        "lint-valid",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("invoke orion-server lint");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    assert!(stdout.contains("is valid"), "stdout={stdout}");
    let _ = std::fs::remove_file(&wf);
}

#[test]
fn lint_invalid_workflow_exits_nonzero_with_field_path() {
    // cache_read requires `connector` — omit it on purpose.
    let wf = write_temp(
        r#"{
        "name": "broken",
        "tasks": [
            {"id":"t1","name":"x","function":{"name":"cache_read","input":{"key":"k"}}}
        ]
    }"#,
        "lint-invalid",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("invoke orion-server lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit, stderr={stderr}"
    );
    assert!(
        stderr.contains("tasks[0].function.input.connector"),
        "stderr should name the missing field, got: {stderr}"
    );
    assert!(
        stderr.contains("REQUIRED"),
        "stderr should mention the REQUIRED code, got: {stderr}"
    );
    let _ = std::fs::remove_file(&wf);
}

#[test]
fn dry_run_executes_workflow_and_prints_trace() {
    let wf = write_temp(
        r#"{
        "name": "dry-run-smoke",
        "tasks": [
            {"id":"t1","name":"log","function":{"name":"log","input":{"message":"hi"}}}
        ]
    }"#,
        "dry-run-wf",
    );
    let input = write_temp(r#"{"x": 1}"#, "dry-run-input");
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("invoke orion-server dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    // Output is JSON with steps[].task_id.
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("dry-run output is not valid JSON: {e}\nstdout={stdout}");
    });
    let steps = parsed["trace"]["steps"]
        .as_array()
        .expect("trace.steps must be an array");
    assert!(steps.iter().any(|s| s["task_id"] == "t1"));
    let _ = std::fs::remove_file(&wf);
    let _ = std::fs::remove_file(&input);
}

fn write_temp_toml(content: &str, suffix: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "orion-cli-test-{}-{}.toml",
        suffix,
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

/// The config layering rule stated in the docs — env override beats config
/// file beats built-in default — asserted end to end through the real
/// binary. This is how every containerized deployment is configured, and
/// until this test the rule had no behavioural coverage
/// (`config_docs_drift_test` only proves the overrides are documented).
#[test]
fn validate_config_layering_env_beats_file_beats_default() {
    let toml = write_temp_toml("[server]\nport = 6111\n", "layering");

    // Built-in default: no file, no env → 8080. The default output is the
    // full effective config as TOML (O15), so the port appears as a setting
    // line, not the old hand-formatted "host:port" summary.
    let out = Command::new(orion_bin())
        .args(["validate-config"])
        .env_remove("ORION_SERVER__PORT")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        stdout.contains("port = 8080"),
        "default port expected: {stdout}"
    );

    // File beats default.
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env_remove("ORION_SERVER__PORT")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        stdout.contains("port = 6111"),
        "file port expected: {stdout}"
    );

    // Env beats file.
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env("ORION_SERVER__PORT", "6222")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        stdout.contains("port = 6222"),
        "env override must beat the file value: {stdout}"
    );

    let _ = std::fs::remove_file(&toml);
}

/// O15: the default TOML dump is serialized from the config structs, so it
/// carries the whole surface — including the sections the hand-maintained
/// summary silently omitted (`[cluster]`, the DLQ knobs, `[trace_storage]`)
/// — and it round-trips: the dump itself must parse as a loadable config.
#[test]
fn validate_config_dumps_the_full_effective_config() {
    let out = Command::new(orion_bin())
        .args(["validate-config"])
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    for needle in [
        "[cluster]",
        "[trace_storage]",
        "dlq_max_retries",
        "[engine.circuit_breaker]",
    ] {
        assert!(
            stdout.contains(needle),
            "the dump must include `{needle}`, the kind of setting the old \
             11-line summary omitted: {stdout}"
        );
    }
    // The validity note must not corrupt the machine-readable stdout.
    toml::from_str::<toml::Value>(&stdout).unwrap_or_else(|e| {
        panic!("validate-config stdout is not valid TOML: {e}\n{stdout}");
    });

    // --format json emits the same tree as JSON.
    let out = Command::new(orion_bin())
        .args(["validate-config", "--format", "json"])
        .env_remove("ORION_SERVER__PORT")
        .output()
        .expect("invoke validate-config --format json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("--format json output is not valid JSON: {e}\n{stdout}");
    });
    assert_eq!(parsed["server"]["port"], 8080, "json dump: {stdout}");

    // --format summary keeps the short human-readable shape.
    let out = Command::new(orion_bin())
        .args(["validate-config", "--format", "summary"])
        .output()
        .expect("invoke validate-config --format summary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        stdout.contains("Configuration is valid.") && stdout.contains("server:"),
        "summary format expected: {stdout}"
    );
}

/// O15: the dump must never print a credential. A password embedded in
/// `storage.url` has to come out masked in every format — the effective-config
/// dump is exactly the thing that gets pasted into tickets and chat.
#[test]
fn validate_config_redacts_url_credentials_in_every_format() {
    let toml = write_temp_toml(
        "[storage]\nurl = \"postgres://orion:sup3rsecret@db.internal:5432/orion\"\n",
        "redaction",
    );

    for format in ["toml", "json", "summary"] {
        let out = Command::new(orion_bin())
            .args(["validate-config", "-c", &toml, "--format", format])
            // Env beats the file, so a stray override would break the URL
            // assertions rather than test the redaction.
            .env_remove("ORION_STORAGE__URL")
            .output()
            .expect("invoke validate-config");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
        assert!(
            !stdout.contains("sup3rsecret") && !stderr.contains("sup3rsecret"),
            "--format {format} leaked the storage.url password: {stdout}"
        );
        assert!(
            stdout.contains("postgres://orion:******@db.internal:5432/orion"),
            "--format {format} should keep the URL readable with the password \
             struck out: {stdout}"
        );
    }

    // Key-named secrets are masked wholesale, not just URL passwords.
    let toml_keys = write_temp_toml(
        "[admin_auth]\nenabled = true\napi_keys = [\"k3y-sup3rsecret-32-chars-long-xx\"]\n",
        "redaction-keys",
    );
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml_keys])
        .env_remove("ORION_ADMIN_AUTH__API_KEYS")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        !stdout.contains("k3y-sup3rsecret"),
        "admin_auth.api_keys must be masked: {stdout}"
    );
    assert!(
        stdout.contains("******"),
        "masked keys should show the mask sentinel: {stdout}"
    );

    let _ = std::fs::remove_file(&toml);
    let _ = std::fs::remove_file(&toml_keys);
}

/// `validate-config` is the pre-boot check operators run in deploy scripts;
/// an invalid config must exit non-zero, not print "valid" and succeed.
#[test]
fn validate_config_rejects_invalid_config_with_nonzero_exit() {
    let toml = write_temp_toml("[server]\nport = 0\n", "invalid");
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "port 0 must fail validation; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("port"),
        "error should name the offending field, got: {stderr}"
    );
    let _ = std::fs::remove_file(&toml);
}

#[test]
fn test_connectivity_reports_ok_for_in_memory_sqlite() {
    let out = Command::new(orion_bin())
        .args(["test-connectivity"])
        .env("ORION_STORAGE__URL", "sqlite::memory:")
        .output()
        .expect("invoke orion-server test-connectivity");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}, stderr={stderr}");
    assert!(
        stdout.contains("storage:") && stdout.contains("OK"),
        "stdout should report storage OK, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// D13: migration numbers are per-backend, so the number alone never says which
// change is pending. The dry-run output has to name the backend and the
// migration, and say so. (Also closes the `migrate` half of T7: the subcommand
// had no test through the binary at all, though it is the documented deploy
// step for every cluster deployment.)
// ---------------------------------------------------------------------------

/// Path to a scratch SQLite file that does not exist yet, plus a cleanup guard.
fn temp_db_url() -> (String, String) {
    let mut path = std::env::temp_dir();
    path.push(format!("orion-cli-migrate-{}.db", uuid::Uuid::new_v4()));
    let p = path.to_string_lossy().into_owned();
    (format!("sqlite:{p}?mode=rwc"), p)
}

fn run_migrate(url: &str, extra: &[&str]) -> (bool, String) {
    let mut args = vec!["migrate"];
    args.extend_from_slice(extra);
    let out = Command::new(orion_bin())
        .args(&args)
        .env("ORION_STORAGE__URL", url)
        .output()
        .expect("invoke orion-server migrate");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

#[test]
fn migrate_dry_run_names_the_backend_and_each_migration() {
    let (url, path) = temp_db_url();

    let (ok, out) = run_migrate(&url, &["--dry-run"]);
    assert!(ok, "dry-run must exit zero: {out}");
    assert!(
        out.contains("Pending migrations on sqlite"),
        "output must name the backend: {out}"
    );
    // The number alone is ambiguous across backends; the name is what
    // identifies the change, so both must be present and paired.
    assert!(
        out.contains("sqlite 001 — initial"),
        "each row must carry backend, number and name: {out}"
    );
    assert!(
        out.contains("not comparable across"),
        "the output must say the numbering is per-backend: {out}"
    );
    // --dry-run must not actually apply anything.
    let (ok, out2) = run_migrate(&url, &["--dry-run"]);
    assert!(ok, "{out2}");
    assert!(
        out2.contains("Pending migrations on sqlite"),
        "dry-run must not have applied the migrations: {out2}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn migrate_applies_then_reports_nothing_pending() {
    let (url, path) = temp_db_url();

    let (ok, out) = run_migrate(&url, &[]);
    assert!(ok, "migrate must exit zero: {out}");
    assert!(
        out.contains("on sqlite") && out.contains("Migrations applied successfully"),
        "{out}"
    );

    // Idempotent: a second run is a no-op that still names the backend, and
    // still exits zero — the Helm pre-upgrade Job runs on every upgrade.
    let (ok, out) = run_migrate(&url, &[]);
    assert!(ok, "re-running migrate must exit zero: {out}");
    assert!(
        out.contains("No pending migrations (sqlite)"),
        "a settled database must say so, and name the backend: {out}"
    );

    let _ = std::fs::remove_file(&path);
}

// ============================================================
// dry-run --stubs, and the `test` runner built on it
// ============================================================

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

/// Without a stub file, a connector-backed task names the stub that would
/// satisfy it — rather than the `Connector '…' not found` the empty
/// function map used to give, which told the author nothing actionable.
#[test]
fn dry_run_without_stubs_names_the_missing_stub() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-missing");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-missing-in");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("no stub for 'http_call' on target 'crm'"),
        "expected the error to name the stub to add, got: {stdout}"
    );
    assert!(
        !out.status.success(),
        "an unsatisfied connector task must fail the dry run"
    );
}

/// With a stub, the whole workflow runs offline and the canned response lands
/// at the task's `output` path.
#[test]
fn dry_run_with_stubs_runs_the_whole_workflow_offline() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-ok");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-ok-in");
    let stubs = write_temp(r#"{"http_call":{"crm":{"name":"Ada"}}}"#, "stub-ok-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "dry run failed: {stdout}");
    assert!(
        stdout.contains("\"customer_name\": \"Ada\""),
        "the stubbed response must reach the downstream task: {stdout}"
    );
}

/// `"*"` stubs any target, for a workflow whose connector name is not worth
/// pinning in the fixture.
#[test]
fn a_wildcard_stub_matches_any_target() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-wild");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-wild-in");
    let stubs = write_temp(r#"{"http_call":{"*":{"name":"Any"}}}"#, "stub-wild-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"customer_name\": \"Any\""));
}

/// A stub file naming a function that cannot be stubbed is a typo, and is
/// refused before anything runs.
#[test]
fn a_stub_file_naming_an_unknown_function_is_refused() {
    let wf = write_temp(CONNECTOR_WORKFLOW, "stub-bad");
    let input = write_temp(r#"{"id":"ORD-1"}"#, "stub-bad-in");
    let stubs = write_temp(r#"{"htp_call":{"crm":{}}}"#, "stub-bad-stubs");

    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input, "--stubs", &stubs])
        .output()
        .expect("run dry-run");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("htp_call"),
        "the error must name the offending key"
    );
}

/// A passing suite exits zero and reports each case.
#[test]
fn test_runner_reports_and_exits_zero_on_a_passing_suite() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("enrich.case.json"),
        r#"{
            "name": "enriches the order",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Ada"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "suite failed: {stdout}");
    assert!(stdout.contains("enriches the order"), "{stdout}");
    assert!(stdout.contains("1 passed, 0 failed"), "{stdout}");
}

/// A failing case prints the diff and exits non-zero, so a suite gates CI.
///
/// The diff is the whole point: "expected X, got Y at this path" is what a
/// bare pass/fail makes an author go and reconstruct by hand.
#[test]
fn test_runner_prints_a_diff_and_exits_nonzero_on_failure() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wrong.case.json"),
        r#"{
            "name": "wrong expectation",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Grace"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!out.status.success(), "a failing case must gate CI");
    assert!(
        stdout.contains("expected \"Grace\", got \"Ada\""),
        "expected a value diff, got: {stdout}"
    );
}

/// Only `*.case.json` is collected from a directory.
///
/// A suite directory is the natural home for the workflows and fixtures the
/// cases reference; scanning every `*.json` reported the workflow under test
/// as a broken case.
#[test]
fn the_runner_ignores_non_case_json_in_the_suite_directory() {
    let scratch = temp_suite();
    let dir = scratch.path();
    // Fixtures that must not be mistaken for cases.
    std::fs::write(dir.join("input.json"), r#"{"id":"ORD-1"}"#).unwrap();
    std::fs::write(dir.join("stubs.json"), r#"{"http_call":{"crm":{}}}"#).unwrap();
    std::fs::write(
        dir.join("ok.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "stubs": {"http_call": {"crm": {"name": "Ada"}}},
            "expect": {"data.order.customer_name": "Ada"}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("1 passed, 0 failed"),
        "wf.json / input.json / stubs.json must not be collected as cases: {stdout}"
    );
}

/// A directory with no cases is an error, not a silent pass — a suite that
/// matched nothing must never look like a green run.
#[test]
fn an_empty_suite_is_an_error() {
    let scratch = temp_suite();
    let dir = scratch.path();
    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no test cases found"),
        "the error must say the suite matched nothing"
    );
}

/// `expect_errors` defaults to empty and is checked even when a case omits it,
/// so a workflow that starts failing its tasks cannot pass silently.
#[test]
fn unexpected_task_errors_fail_a_case() {
    let scratch = temp_suite();
    let dir = scratch.path();
    std::fs::write(
        dir.join("unstubbed.case.json"),
        r#"{
            "name": "no stub supplied",
            "workflow": "wf.json",
            "input": {"id": "ORD-1"},
            "expect": {}
        }"#,
    )
    .unwrap();

    let out = Command::new(orion_bin())
        .args(["test", dir.to_str().unwrap()])
        .output()
        .expect("run test");
    assert!(
        !out.status.success(),
        "a case whose workflow errored must not pass: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// T7: the one subcommand with no test through the binary. Its *content* is
/// drift-guarded by `committed_openapi_json_is_up_to_date`, but that test
/// generates the spec in-process — nothing proved the subcommand itself exits
/// zero and writes the spec to stdout, which is exactly how CONTRIBUTING
/// tells contributors to regenerate `docs/openapi.json`.
#[test]
fn dump_openapi_writes_the_spec_to_stdout() {
    let out = Command::new(orion_bin())
        .arg("dump-openapi")
        .output()
        .expect("invoke orion-server dump-openapi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr={stderr}");

    // Stdout must be the spec and nothing else — a stray log line would
    // corrupt every `dump-openapi > docs/openapi.json` redirect.
    let spec: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be a single JSON document");
    assert_eq!(
        spec["openapi"], "3.1.0",
        "spec version: {}",
        spec["openapi"]
    );
    assert!(
        spec["paths"]["/api/v1/data/{channel}"].is_object(),
        "the data plane must be documented"
    );
}

/// T38: `preflight`'s documented contract is "exits non-zero when it finds
/// anything, so it can gate a deploy" — and nothing invoked it as a binary,
/// so that exit-code promise (the entire point of the subcommand) was
/// unverified. `preflight_test.rs` drives the library; this drives the
/// process a deploy pipeline actually runs.
#[tokio::test]
async fn preflight_binary_exit_code_gates_a_deploy() {
    // A migrated SQLite file with nothing stored: preflight must exit 0.
    let dir = crate::common::ScratchDir::new("preflight_cli");
    let url = dir.url();
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: url.clone(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .expect("migrate scratch db");

    let clean = Command::new(orion_bin())
        .env("ORION_STORAGE__URL", &url)
        .arg("preflight")
        .output()
        .expect("invoke orion-server preflight");
    assert!(
        clean.status.success(),
        "a clean store must pass preflight: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // Store a channel whose config carries the retired pre-1.0 `cors` key —
    // the exact class of break preflight exists to find.
    let orion::storage::DbPool::Sqlite(sq) = &pool else {
        panic!("sqlite expected");
    };
    sqlx::query(
        "INSERT INTO channels \
           (channel_id, version, name, channel_type, protocol, methods_json, route_pattern, status, config_json) \
         VALUES ('legacy', 1, 'legacy', 'sync', 'http', '[\"POST\"]', '/legacy', 'active', '{\"cors\": {}}')",
    )
    .execute(sq)
    .await
    .expect("seed broken channel");
    drop(pool);

    let dirty = Command::new(orion_bin())
        .env("ORION_STORAGE__URL", &url)
        .arg("preflight")
        .output()
        .expect("invoke orion-server preflight");
    let stdout = String::from_utf8_lossy(&dirty.stdout);
    let stderr = String::from_utf8_lossy(&dirty.stderr);
    assert!(
        !dirty.status.success(),
        "a store with a 1.0 break must fail the gate: stdout={stdout} stderr={stderr}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("legacy"),
        "the finding must name the channel: stdout={stdout} stderr={stderr}"
    );
}

// ============================================================
// #283: case metadata, the recorded call log, and the expect roots
// ============================================================

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
            "name":"mongo_write","input":{
                "connector":"sessions","database":"app","collection":"sessions",
                "op":"update_one",
                "output":"data.write_result",
                "filter":{"_id":{"var":"temp_data.sid"}},
                "update":{"$set":{"generation":{"if":[true,2,1]},"revokedAt":null}}}}}
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

/// The headline of #283: a case can set `metadata`, so a header-gated branch is
/// reachable offline. Also pins the two fidelity rules that make an offline
/// pass mean a production pass — lowercased keys, masked credentials.
#[test]
fn a_case_can_set_metadata_and_reach_a_header_gated_branch() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("device.case.json"),
        r#"{
            "name": "device login",
            "workflow": "wf.json",
            "metadata": {
                "headers": {"DeviceId": "device-abc", "Authorization": "Bearer s3cret"},
                "auth": {"claims": {"sub": "asha@example.com"}},
                "query": {"page": "2"}
            },
            "input": {},
            "expect": {
                "data.mode": "device",
                "data.device": "device-abc",
                "data.subject": "asha@example.com",
                "data.page": "2",
                "data.token": "******"
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "the device branch must be reachable offline: {out}");
}

/// A metadata shape the HTTP ingress could never produce fails the case, naming
/// the field — not silently, and not as a mystery `<absent>` diff later.
#[test]
fn malformed_case_metadata_fails_naming_the_field() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("bad.case.json"),
        r#"{
            "workflow": "wf.json",
            "metadata": {"headers": ["nope"]},
            "input": {},
            "expect": {}
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "a malformed metadata block must fail the case: {out}");
    assert!(out.contains("metadata.headers"), "{out}");
}

/// `expect` reaches the other four documents, not just `data`.
#[test]
fn expect_reaches_metadata_temp_data_calls_and_the_audit_trail() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("roots.case.json"),
        r#"{
            "workflow": "wf.json",
            "metadata": {"channel": "rotate"},
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect": {
                "data.done": true,
                "metadata.channel": "rotate",
                "temp_data.sid": "sess-1",
                "calls.mongo_write[0].stub_target": "sessions",
                "calls.mongo_write[0].input.filter._id": "sess-1",
                "calls.mongo_write[0].task_id": "persist",
                "audit_trail[1].task_id": "persist"
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(ok, "every root must resolve: {out}");
}

/// The regression #283 is about. The workflow writes `{"if": [...]}` verbatim
/// because `mongo_write` folds `{"var": ..}` and nothing else; a case asserting
/// the intended number now fails where a stubbed run used to stay green.
#[test]
fn expect_calls_catches_jsonlogic_written_verbatim() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("rotate.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect_calls": {
                "mongo_write": [
                    {"collection": "sessions",
                     "update": {"$set": {"generation": 2, "revokedAt": null}}}
                ]
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "the verbatim-logic write must be caught: {out}");
    assert!(
        out.contains("generation"),
        "the diff must name the field that was not what it looks like: {out}"
    );
}

/// Presence is strict in `expect_calls`, unlike `expect`: `null` asserts
/// *written as null*, so a field the workflow never wrote is a failure rather
/// than a pass. That distinction is the whole point of the session-lifecycle
/// case in the issue.
#[test]
fn expect_calls_treats_a_null_expectation_as_written_not_absent() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("absent.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect_calls": {
                "mongo_write": [{"update": {"$set": {"neverWritten": null}}}]
            }
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "an unwritten field must not pass as null: {out}");
    assert!(out.contains("not written"), "{out}");
}

/// The count is part of the assertion, so an unexpected extra call fails — and
/// an empty list asserts a function was never called at all.
#[test]
fn expect_calls_checks_the_call_count() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("count.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect_calls": {"mongo_write": [], "publish_kafka": []}
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(
        !ok,
        "one recorded write against zero expected must fail: {out}"
    );
    assert!(
        out.contains("expected 0 call(s), recorded 1"),
        "the diff must give both counts: {out}"
    );
}

/// Branch coverage in one line — and it fails when a condition sends the run
/// down a different path.
#[test]
fn expect_tasks_asserts_which_tasks_ran() {
    let scratch = suite_with(VERBATIM_LOGIC_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("ran.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect_tasks": ["seed", "persist"]
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wrong.case.json"),
        r#"{
            "workflow": "wf.json",
            "input": {},
            "stubs": {"mongo_write": {"sessions": {"modified": 1}}},
            "expect_tasks": ["seed"]
        }"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "the wrong expectation must fail: {out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    assert!(out.contains("tasks: expected"), "{out}");
}

/// The root is required. A bare path used to mean `data.` and is now refused
/// before the workflow runs, with the fix in the message.
#[test]
fn an_unrooted_expect_path_is_refused_with_the_fix() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("bare.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {"mode": "password"}}"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(!ok, "an unrooted path must fail: {out}");
    assert!(out.contains("did you mean 'data.mode'"), "{out}");
    assert!(out.contains("temp_data"), "the roots must be listed: {out}");
}

/// The silent miss the roots exist to remove: `metadata.absent` used to resolve
/// as `data.metadata.absent`, come back absent, and — because an expected null
/// matches absent — *pass*. A typo'd root is the same class and now fails too.
#[test]
fn a_typo_in_the_root_no_longer_passes_silently() {
    let scratch = suite_with(HEADER_BRANCH_WORKFLOW);
    let dir = scratch.path();
    std::fs::write(
        dir.join("typo.case.json"),
        r#"{"workflow": "wf.json", "input": {}, "expect": {"dat.mode": null}}"#,
    )
    .unwrap();

    let (ok, out) = run_suite(dir);
    assert!(
        !ok,
        "a typo'd root expecting null must not pass silently: {out}"
    );
    assert!(out.contains("has no root"), "{out}");
}

/// `dry-run` publishes the same roots the case format uses, so a path read off
/// a dry run can be pasted into a case unchanged.
#[test]
fn dry_run_prints_the_call_log_and_the_context_documents() {
    let wf = write_temp(VERBATIM_LOGIC_WORKFLOW, "dryrun-calls");
    let input = write_temp("{}", "dryrun-calls-in");
    let stubs = write_temp(
        r#"{"mongo_write":{"sessions":{"modified":1}}}"#,
        "dryrun-stubs",
    );
    let metadata = write_temp(r#"{"channel":"rotate"}"#, "dryrun-meta");

    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            &wf,
            "-i",
            &input,
            "--stubs",
            &stubs,
            "--metadata",
            &metadata,
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("dry-run prints JSON");
    assert_eq!(parsed["metadata"]["channel"], "rotate");
    assert_eq!(parsed["temp_data"]["sid"], "sess-1");
    // Grouped by function, the same shape a case's `calls.<fn>[i]` addresses —
    // one shape on both surfaces, so a path lifted from here works in a case.
    // Global ordering is not lost to the grouping: each record carries `seq`.
    assert_eq!(parsed["calls"]["mongo_write"][0]["function"], "mongo_write");
    assert_eq!(parsed["calls"]["mongo_write"][0]["task_id"], "persist");
    assert_eq!(parsed["calls"]["mongo_write"][0]["seq"], 0);
    assert_eq!(
        parsed["calls"]["mongo_write"][0]["input"]["update"]["$set"]["generation"],
        serde_json::json!({"if": [true, 2, 1]}),
        "the log shows the node Mongo would have stored"
    );
    assert!(parsed["audit_trail"].is_array());

    for path in [&wf, &input, &stubs, &metadata] {
        let _ = std::fs::remove_file(path);
    }
}

/// The warning is advisory by default, so no existing pipeline breaks on
/// upgrade — and `--deny-warnings` is the opt-in that gates a PR.
#[test]
fn lint_warns_on_unresolvable_logic_and_denies_it_on_request() {
    let wf = write_temp(VERBATIM_LOGIC_WORKFLOW, "lint-logic");

    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a warning must not fail lint by default: {stderr}"
    );
    assert!(stderr.contains("warning:"), "{stderr}");
    assert!(
        stderr.contains("'if'"),
        "the operator must be named: {stderr}"
    );

    let out = Command::new(orion_bin())
        .args(["lint", &wf, "--deny-warnings"])
        .output()
        .expect("run lint --deny-warnings");
    assert!(
        !out.status.success(),
        "--deny-warnings must gate: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_file(&wf);
}

// ============================================================
// #286: lint a definition set, not one file at a time
// ============================================================

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

/// #286's own reproduction: a workflow that lints clean per-file because both
/// of its dangling references live in files `lint <file>` never opens.
#[test]
fn set_mode_catches_references_a_single_file_lint_cannot_see() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("probe.json"),
        r#"{"workflow_id":"probe","name":"probe","tasks":[
            {"id":"c","name":"c","function":{"name":"mongo_read","input":{
              "connector":"does-not-exist","database":"x","collection":"y",
              "filter":{},"output":"temp_data.z"}}},
            {"id":"d","name":"d","function":{"name":"channel_call","input":{
              "channel":"no-such-channel","data_logic":{},"output":"temp_data.q"}}}]}"#,
    )
    .unwrap();

    // Per-file: unchanged, still clean — that is the bug being demonstrated.
    let out = Command::new(orion_bin())
        .args(["lint", dir.join("probe.json").to_str().unwrap()])
        .output()
        .expect("run lint");
    assert!(
        out.status.success(),
        "per-file lint must keep its behaviour"
    );

    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "set mode must fail: {report}");
    assert!(report.contains("closure.connector"), "{report}");
    assert!(report.contains("closure.channel_call"), "{report}");
}

/// A reference the set deliberately does not contain is declarable, the way a
/// package declares `requires`.
#[test]
fn a_boundary_admits_a_reference_the_set_does_not_contain() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"d","name":"d","function":{"name":"channel_call","input":{
              "channel":"deployed-elsewhere","data_logic":{},"output":"temp_data.q"}}}]}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "empty boundary is the default: {report}");

    let (ok, report) = lint_dir(dir, &["--requires-channel", "deployed-elsewhere"]);
    assert!(ok, "a declared boundary must satisfy closure: {report}");
}

/// Entities are found by shape, and a fixture beside them is reported as
/// skipped rather than silently dropped or misread as a broken entity.
#[test]
fn non_entities_are_reported_not_silently_ignored() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(
        dir.join("nested/wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[{"id":"t1","name":"t1","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    // The shape `examples/packages/*/request.json` has.
    std::fs::write(dir.join("request.json"), r#"{"data":{"amount":5}}"#).unwrap();
    std::fs::write(dir.join("broken.json"), r#"{oops"#).unwrap();

    let (_, report) = lint_dir(dir, &[]);
    assert!(
        report.contains("request.json is not a channel, workflow or connector"),
        "a skipped file must be named: {report}"
    );
    assert!(
        report.contains("broken.json is not readable JSON"),
        "a file that does not parse is a likely mistake, not a quiet skip: {report}"
    );
    assert!(
        report.contains("1 workflow(s)"),
        "the nested entity must be found — a one-level walk would miss it: {report}"
    );
}

/// A directory that yields nothing is an error, never a green run — the same
/// rule `orion-server test` applies to a suite that matched no cases.
#[test]
fn a_directory_with_no_definitions_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(dir.join("notes.md"), "nothing here").unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "an empty set must not look like a pass: {report}");
    assert!(report.contains("no definitions found"), "{report}");
}

/// Two channels claiming one route are served by whichever loads second,
/// which is not a property anyone chose.
#[test]
fn a_route_claimed_twice_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[{"id":"t1","name":"t1","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    for (i, name) in ["a", "b"].iter().enumerate() {
        std::fs::write(
            dir.join(format!("ch{i}.json")),
            format!(
                r#"{{"channel_id":"c{i}","name":"{name}","channel_type":"sync","protocol":"rest",
                    "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}}"#
            ),
        )
        .unwrap();
    }
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("duplicate.route_pattern"), "{report}");
    assert!(
        report.contains("GET /users"),
        "the diff must name the route: {report}"
    );
}

/// A connector of the wrong type parses, imports, activates, and fails at the
/// first request. Set mode is where it can be caught offline.
#[test]
fn a_connector_of_the_wrong_type_is_an_error() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"pg","connector_type":"db",
            "config":{"connection_string":"postgres://h/d"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"http_call","input":{
              "connector":"pg","method":"GET","path":"/x","output":"data.r"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("type.connector"), "{report}");
    assert!(
        report.contains("needs a http connector"),
        "the message must name both types: {report}"
    );
}

/// Warnings do not fail set mode, and `--deny-warnings` promotes them — the
/// same flag and the same meaning it already has for a single file.
#[test]
fn set_mode_warnings_gate_only_under_deny_warnings() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"t","name":"t","function":{"name":"mongo_write","input":{
              "connector":"m","database":"d","collection":"c","op":"insert_one",
              "output":"data.r",
              "document":{"at":{"cat":["a","b"]}}}}}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"m","connector_type":"db","config":{"connection_string":"mongodb://h/d"}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "a warning must not fail set mode: {report}");
    assert!(report.contains("logic.unresolvable"), "{report}");

    let (ok, _) = lint_dir(dir, &["--deny-warnings"]);
    assert!(
        !ok,
        "--deny-warnings must gate the same way it does per-file"
    );
}

/// The repository's own packages are a real definition set and must lint
/// clean — the closure checks resolve `channel_call` and connector references
/// across nine packages.
#[test]
fn the_shipped_examples_lint_clean_as_a_set() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/packages");
    let (ok, report) = lint_dir(&dir, &[]);
    assert!(
        ok,
        "the shipped packages must lint clean as a set: {report}"
    );
    assert!(report.contains("0 error(s)"), "{report}");
}

// ============================================================
// #285: shared definition sources — fragments and a value catalog
// ============================================================

/// A definitions directory holding a value catalog and one fragment.
fn temp_defs_with_shared() -> ScratchDir {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::create_dir_all(dir.join("fragments")).unwrap();
    std::fs::write(
        dir.join("common.json"),
        r#"{ "constants": { "db": { "connector": "mongo", "database": "app" } },
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

/// The whole point: one catalog entry, spliced, so an error string cannot
/// drift into three spellings across the set.
#[test]
fn a_shared_reference_expands_and_runs() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","condition":true,"tasks":[
            {"id":"g","use":"deny","with":{"message":"Please sign in again."}},
            {"id":"err","name":"Error body","function":{"name":"map","input":{"mappings":[
               {"path":"data.out","logic":{"$from":"errors.USER_NOT_FOUND"}}]}}}]}"#,
    )
    .unwrap();
    let input = write_temp("{}", "shared-in");

    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(
        parsed["data"]["denied"], "Please sign in again.",
        "the call site's argument must beat the fragment's default"
    );
    assert_eq!(parsed["data"]["out"]["body"], "User Not Found !");
    // Namespaced, so two instances of one fragment cannot collide.
    assert_eq!(parsed["calls"], serde_json::json!({}));
    assert_eq!(parsed["trace"]["steps"][0]["task_id"], "g.write");

    let _ = std::fs::remove_file(&input);
}

/// Without a catalog, an unexpanded reference reaches validation as a task
/// missing its `name` and `function` — an error describing the symptom. The
/// command names the cause instead.
#[test]
fn a_reference_without_a_catalog_names_the_cause() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","tasks":[{"id":"g","use":"deny"}]}"#,
    )
    .unwrap();
    let out = Command::new(orion_bin())
        .args(["lint", dir.join("wf.json").to_str().unwrap()])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("fragment 'deny'") && stderr.contains("--definitions"),
        "the error must name the reference and the missing flag: {stderr}"
    );
}

/// A typo'd reference fails at lint, which is the whole reason the set lint
/// (#286) is this feature's prerequisite.
#[test]
fn an_unresolvable_reference_fails_the_set_lint() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"r","name":"r","function":{"name":"mongo_read","input":{
               "$from":"constants.dbb","collection":"users","filter":{},
               "output":"temp_data.u"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("closure.shared_value"), "{report}");
    assert!(report.contains("constants.dbb"), "{report}");
}

/// In set mode the catalog is found without a flag, and the splice satisfies
/// the schema — `connector` and `database` come from the shared value, so a
/// failure to splice would surface as a REQUIRED error.
#[test]
fn set_mode_resolves_the_catalog_without_a_flag() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"mongo","connector_type":"db",
            "config":{"connection_string":"mongodb://h/app"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"r","name":"r","function":{"name":"mongo_read","input":{
               "$from":"constants.db","collection":"users","filter":{},
               "output":"temp_data.u"}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(
        ok,
        "the spliced connector must satisfy the schema: {report}"
    );
    assert!(
        report.contains("2 shared value(s), 1 fragment(s)"),
        "the summary must report the catalog: {report}"
    );
}

/// A sibling key overrides the shared value, which is what lets one field be
/// changed at a call site without copying the rest.
#[test]
fn a_call_site_can_override_one_field_of_a_shared_value() {
    let scratch = temp_defs_with_shared();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"name":"w","condition":true,"tasks":[
            {"id":"m","name":"m","function":{"name":"map","input":{"mappings":[
               {"path":"data.x","logic":{"$from":"constants.db","database":"other"}}]}}}]}"#,
    )
    .unwrap();
    let input = write_temp("{}", "override-in");
    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("JSON");
    assert_eq!(parsed["data"]["x"]["database"], "other", "sibling wins");
    assert_eq!(
        parsed["data"]["x"]["connector"], "mongo",
        "the rest is spliced"
    );
    let _ = std::fs::remove_file(&input);
}

// ============================================================
// dataflow-rs 3.6: task groups and `terminal`
// ============================================================

/// The guard clause the upgrade exists for: a terminal group ends the
/// workflow, so nothing after it needs a hand-written negation.
#[test]
fn a_terminal_task_group_ends_the_workflow() {
    let wf = write_temp(
        r#"{"name":"guard","condition":true,"tasks":[
            {"id":"seed","name":"Seed","function":{"name":"map","input":{"mappings":[
               {"path":"data.user","logic":null}]}}},
            {"id":"not_found","condition":{"==":[{"var":"data.user"},null]},
             "terminal":true,"tasks":[
               {"id":"body","name":"404","function":{"name":"map","input":{"mappings":[
                  {"path":"data.status","logic":404}]}}}]},
            {"id":"never","name":"Never","function":{"name":"map","input":{"mappings":[
               {"path":"data.reached","logic":true}]}}}]}"#,
        "group-terminal",
    );
    let input = write_temp("{}", "group-terminal-in");
    let out = Command::new(orion_bin())
        .args(["dry-run", "-w", &wf, "-i", &input])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(parsed["data"]["status"], 404, "the group's task ran");
    assert!(
        parsed["data"].get("reached").is_none(),
        "a terminal group must end the workflow: {}",
        parsed["data"]
    );
    for p in [&wf, &input] {
        let _ = std::fs::remove_file(p);
    }
}

/// A task inside a group is a task. Every check that walks the array has to
/// see it, or a guarded half of a workflow ships unvalidated.
#[test]
fn tasks_inside_a_group_are_validated_with_nested_paths() {
    let wf = write_temp(
        r#"{"name":"w","tasks":[
            {"id":"g","condition":true,"tasks":[
               {"id":"bad","name":"bad","function":{"name":"mongo_writes","input":{}}}]}]}"#,
        "group-validate",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("tasks[0].tasks[0].function.name"),
        "the path must address the nested task: {stderr}"
    );
    assert!(
        stderr.contains("did you mean 'mongo_write'?"),
        "and the suggestion must still fire inside a group: {stderr}"
    );
    let _ = std::fs::remove_file(&wf);
}

/// Closure checking reaches inside groups, or a connector referenced only
/// from a guard clause passes a set lint that exists to catch exactly that.
#[test]
fn closure_checking_reaches_inside_a_group() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"w","name":"w","tasks":[
            {"id":"g","condition":true,"tasks":[
               {"id":"r","name":"r","function":{"name":"mongo_read","input":{
                  "connector":"nowhere","database":"d","collection":"c",
                  "filter":{},"output":"temp_data.z"}}}]}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok, "{report}");
    assert!(report.contains("closure.connector"), "{report}");
    assert!(report.contains("nowhere"), "{report}");
}

/// Groups and tasks share one id namespace — the engine refuses a collision at
/// build, which for Orion is a failed reload rather than one bad workflow.
#[test]
fn a_group_id_colliding_with_a_task_id_is_refused() {
    let wf = write_temp(
        r#"{"name":"w","tasks":[
            {"id":"dup","name":"t","function":{"name":"map","input":{"mappings":[]}}},
            {"id":"dup","condition":true,"tasks":[
               {"id":"inner","name":"i","function":{"name":"map","input":{"mappings":[]}}}]}]}"#,
        "group-dup-id",
    );
    let out = Command::new(orion_bin())
        .args(["lint", &wf])
        .output()
        .expect("run lint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("DUPLICATE_TASK_ID"), "{stderr}");
    let _ = std::fs::remove_file(&wf);
}

/// Every workflow written before 3.6 is a flat array, and none of them may
/// change behaviour.
#[test]
fn a_flat_workflow_is_unaffected_by_the_step_walk() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/packages");
    let (ok, report) = lint_dir(&dir, &[]);
    assert!(ok, "the shipped packages must still lint clean: {report}");
}

/// `env://` is the documented way to author a connector secret, so the
/// inventory line the set lint prints for one must not gate a pipeline.
/// Counting it as a warning left `--deny-warnings` failing on every real set,
/// which is the same as not shipping the flag.
#[test]
fn an_env_reference_is_reported_without_failing_deny_warnings() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"crm","connector_type":"http","config":{
             "url":"https://example.com",
             "headers":{"Authorization":"env://CRM_TOKEN"}}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "a clean set must lint clean: {report}");
    assert!(
        report.contains("CRM_TOKEN"),
        "the environment inventory is the point of the line: {report}"
    );
    assert!(
        report.contains("0 warning(s)"),
        "an inventory note is not a warning: {report}"
    );

    let (ok, report) = lint_dir(dir, &["--deny-warnings"]);
    assert!(
        ok,
        "--deny-warnings must not gate on an env:// inventory note: {report}"
    );
}

/// The inventory covers every scheme this build can resolve, not just
/// `env://` — a set authored against Vault would otherwise report clean and
/// then be missing a secret at deploy. And it uses the masking policy's strict
/// predicate, so a `postgres://user:password@host` connection string is a
/// connection string, not a secret reference.
#[test]
fn the_secret_inventory_covers_every_resolvable_scheme() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn.json"),
        r#"{"name":"db","connector_type":"db","config":{
             "connection_string":"postgres://user:password@host/db",
             "options":{"ca":"vault://secret/db#ca"}}}"#,
    )
    .unwrap();

    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "{report}");
    assert!(
        report.contains("vault://secret/db#ca"),
        "a vault reference belongs in the inventory: {report}"
    );
    assert!(
        !report.contains("postgres://user:password@host"),
        "a connection string is not a secret reference: {report}"
    );
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

/// The lint projects a route the way the route table does, so the gate and the
/// thing being gated agree. Each of these disagreed while the lint compared
/// pattern strings and folded methods into an "ANY" sentinel of its own.
#[test]
fn route_collisions_are_judged_the_way_the_route_table_judges_them() {
    // Parameter *names* do not distinguish routes: `/o/{id}` and
    // `/o/{orderId}` are one entry in the table, so they collide.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/o/{id}","methods":["GET"],"workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/o/{orderId}","methods":["GET"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        !ok && report.contains("duplicate.route_pattern"),
        "differing param names are the same route: {report}"
    );

    // No `methods` means every method, so it overlaps one that names GET.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        !ok && report.contains("duplicate.route_pattern"),
        "an unrestricted channel claims every method: {report}"
    );

    // Different priorities are a deliberate override, which activation allows.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w","priority":0}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w","priority":10}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(
        ok,
        "a higher-priority override activates fine and must lint clean: {report}"
    );

    // Disjoint methods on one pattern never match the same request.
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["POST"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(ok, "disjoint methods do not collide: {report}");
}

/// A Kafka channel registers no HTTP route, so a stray `route_pattern` on one
/// is not a claim on anything and must not collide.
#[test]
fn a_non_rest_channel_claims_no_route() {
    let scratch = defs_with_channels(&[
        r#"{"channel_id":"c0","name":"a","channel_type":"async","protocol":"kafka",
            "topic":"t0","route_pattern":"/users","workflow_id":"w"}"#,
        r#"{"channel_id":"c1","name":"b","channel_type":"sync","protocol":"rest",
            "route_pattern":"/users","methods":["GET"],"workflow_id":"w"}"#,
    ]);
    let (ok, report) = lint_dir(scratch.path(), &[]);
    assert!(ok, "a kafka channel serves no route: {report}");
}

/// #294: a fragment's ids are namespaced by the call site so it "cannot
/// collide with the including workflow, or with a second instance of itself".
/// That held only while every step was a plain task — a fragment holding a
/// **task group** had the group's own id rewritten and the ids inside it
/// emitted verbatim, into the host workflow's namespace.
///
/// Both of the issue's reproductions, run end to end. They failed with
/// `DUPLICATE_TASK_ID`, whose own message notes it "fails the entire engine
/// reload rather than just this workflow" — and the author could not see it
/// coming, because the colliding name is private to the fragment.
#[test]
fn a_fragments_nested_ids_are_namespaced_by_the_call_site() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("shared.json"),
        r#"{ "fragments": { "guard": { "tasks": [
             { "id": "probe", "name": "Probe", "function": { "name": "map",
                 "input": { "mappings": [ { "path": "temp_data.p", "logic": 1 } ] } } },
             { "id": "span", "condition": true, "tasks": [
                 { "id": "inner", "name": "Inner", "function": { "name": "map",
                     "input": { "mappings": [ { "path": "temp_data.i", "logic": 1 } ] } } } ] } ] } } }"#,
    )
    .unwrap();

    // (1) two instances of one fragment.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" }, { "id": "b", "use": "guard" } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(
        ok,
        "two instances of one fragment must not collide:\n{report}"
    );

    // (2) one instance, against a host task sharing a nested task's name.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" },
                        { "id": "inner", "name": "The workflow's own task",
                          "function": { "name": "map", "input": { "mappings": [
                            { "path": "temp_data.own", "logic": 1 } ] } } } ] }"#,
    )
    .unwrap();
    let (ok, report) = lint_with_definitions(dir);
    assert!(
        ok,
        "a fragment's nested id must not collide with the host workflow's own:\n{report}"
    );

    // And the ids the engine actually runs are the namespaced ones — the
    // check the issue made against a live 1.2.0 with `expect_tasks`, which
    // reported `_session.call` namespaced beside a bare `_session_deny`.
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "repro", "name": "Repro",
             "tasks": [ { "id": "a", "use": "guard" }, { "id": "b", "use": "guard" } ] }"#,
    )
    .unwrap();
    let input = write_temp("{}", "i294");
    let out = Command::new(orion_bin())
        .args([
            "dry-run",
            "-w",
            dir.join("wf.json").to_str().unwrap(),
            "-i",
            &input,
            "--definitions",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let ran: Vec<&str> = parsed["trace"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|s| s["task_id"].as_str())
        .collect();
    assert_eq!(ran, ["a.probe", "a.inner", "b.probe", "b.inner"]);
    let _ = std::fs::remove_file(&input);
}

/// The other half of #294. The no-nested-fragments rule read only a
/// fragment's top-level steps, so a `use` inside a group was neither refused
/// nor expanded: it survived into the host workflow, where it is a step the
/// engine cannot parse. The restriction must hold at every depth.
#[test]
fn a_fragment_including_a_fragment_inside_a_group_is_refused() {
    let scratch = temp_defs();
    let dir = scratch.path();
    std::fs::write(
        dir.join("shared.json"),
        r#"{ "fragments": {
             "leaf": { "tasks": [ { "id": "l", "name": "Leaf", "function": { "name": "map",
                 "input": { "mappings": [ { "path": "temp_data.l", "logic": 1 } ] } } } ] },
             "outer": { "tasks": [
                 { "id": "span", "condition": true, "tasks": [
                     { "id": "nested", "use": "leaf" } ] } ] } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "nest", "name": "Nest", "tasks": [ { "id": "a", "use": "outer" } ] }"#,
    )
    .unwrap();

    let (ok, report) = lint_with_definitions(dir);
    assert!(!ok, "{report}");
    assert!(
        report.contains("shared.fragment_nested")
            && report.contains("a fragment cannot include another fragment"),
        "the restriction must be reported where it bites, rather than surfacing \
         as an uncompiled reference against a set that can actually resolve it: {report}"
    );
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
