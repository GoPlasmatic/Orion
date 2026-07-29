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
