//! A6 (Tier-1 ergonomics): CLI subcommands `lint`, `dry-run`, `test-connectivity`.
//!
//! Snapshot-style tests against the compiled binary. The binary path is
//! resolved from cargo's CARGO_BIN_EXE_<name> env var so these run on
//! whatever target was just built.

use std::process::Command;

fn orion_bin() -> String {
    // Resolved at compile time: cargo guarantees CARGO_BIN_EXE_<name> to `env!`
    // for integration tests (stable since Rust 1.43). Reading it at runtime via
    // std::env::var is fragile — cargo only began exporting it into the test
    // process environment in toolchains newer than 1.88.
    env!("CARGO_BIN_EXE_orion-server").to_string()
}

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
/// until this test the rule had no behavioral coverage
/// (`config_docs_drift_test` only proves the overrides are documented).
#[test]
fn validate_config_layering_env_beats_file_beats_default() {
    let toml = write_temp_toml("[server]\nport = 6111\n", "layering");

    // Built-in default: no file, no env → 8080.
    let out = Command::new(orion_bin())
        .args(["validate-config"])
        .env_remove("ORION_SERVER__PORT")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(stdout.contains(":8080"), "default port expected: {stdout}");

    // File beats default.
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env_remove("ORION_SERVER__PORT")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(stdout.contains(":6111"), "file port expected: {stdout}");

    // Env beats file.
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env("ORION_SERVER__PORT", "6222")
        .output()
        .expect("invoke validate-config");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout={stdout}");
    assert!(
        stdout.contains(":6222"),
        "env override must beat the file value: {stdout}"
    );

    let _ = std::fs::remove_file(&toml);
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
