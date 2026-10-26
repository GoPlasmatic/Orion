//! The server binary's own subcommands — `validate-config`, `migrate`,
//! `test-connectivity` and `dump-openapi` — against the compiled binary
//! (cargo's CARGO_BIN_EXE_<name>). The offline authoring tools are
//! `orion-cli` subcommands, tested in that crate.

use std::process::Command;

use crate::common::orion_bin;

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

/// `${VAR:?message}` stops `validate-config` with one line naming the
/// variable, the reason and the position — and a placeholder that only a
/// comment mentions requires nothing.
#[test]
fn validate_config_reports_a_required_with_message_failure() {
    let toml = write_temp_toml(
        "# the url below reads ${ORION_TEST_ONLY_IN_A_COMMENT}\n\
         [storage]\n\
         url = \"${ORION_TEST_STATE_DB:?set it to the state database}\"\n",
        "required-message",
    );
    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env_remove("ORION_TEST_STATE_DB")
        .output()
        .expect("invoke validate-config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "stderr={stderr}");
    assert!(
        stderr.contains("ORION_TEST_STATE_DB is required: set it to the state database"),
        "{stderr}"
    );
    assert!(stderr.contains(":3:8)"), "{stderr}");
    assert!(!stderr.contains("ORION_TEST_ONLY_IN_A_COMMENT"), "{stderr}");

    let out = Command::new(orion_bin())
        .args(["validate-config", "-c", &toml])
        .env("ORION_TEST_STATE_DB", "sqlite::memory:")
        .output()
        .expect("invoke validate-config");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
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
        out.contains("sqlite 001 — v2 baseline"),
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

/// An Orion 1.x state database carries migrations the v2 baseline replaced.
/// `migrate` refuses it by name, both to apply and to plan, rather than leaving
/// sqlx to report an unknown version number.
#[tokio::test]
async fn migrate_refuses_a_1x_database_by_name() {
    let (url, path) = temp_db_url();
    {
        let pool = sqlx::SqlitePool::connect(&url).await.expect("sqlite");
        sqlx::raw_sql(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, execution_time BIGINT NOT NULL);
             INSERT INTO _sqlx_migrations VALUES (1, 'initial', CURRENT_TIMESTAMP, 1, x'00', 0);
             INSERT INTO _sqlx_migrations VALUES (25, 'cluster shared state', CURRENT_TIMESTAMP, 1, x'00', 0);",
        )
        .execute(&pool)
        .await
        .expect("seed a 1.x migration ledger");
        pool.close().await;
    }

    for extra in [&[][..], &["--dry-run"][..]] {
        let (ok, out) = run_migrate(&url, extra);
        assert!(!ok, "a 1.x database must be refused ({extra:?}): {out}");
        assert!(
            out.contains("Orion 1.x database cannot be upgraded") && out.contains("[25]"),
            "the refusal names what the database is and the stray version: {out}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// A Postgres URL on a port nothing listens on: every attempt is refused,
/// which sqlx reports as a pool timeout after `acquire_timeout_secs`.
fn closed_postgres_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("postgres://orion:orion@127.0.0.1:{port}/orion")
}

/// #346: `--wait` retries a database that is not accepting connections,
/// says so on stderr each time, and gives up past the window naming how
/// long it waited and the last error.
#[test]
fn migrate_wait_gives_up_with_the_last_connection_error() {
    let started = std::time::Instant::now();
    let out = Command::new(orion_bin())
        .args(["migrate", "--wait", "2s"])
        .env("ORION_STORAGE__URL", closed_postgres_url())
        .env("ORION_STORAGE__ACQUIRE_TIMEOUT_SECS", "1")
        .output()
        .expect("invoke orion-server migrate");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "the window must be waited out (elapsed {:?}): {stderr}",
        started.elapsed()
    );
    assert!(
        stderr.contains("waiting for the state database (not accepting connections within 1s)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("state database not reachable after"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("orion:orion@"),
        "a progress line must never print the URL: {stderr}"
    );
}

/// SQLite's failures do not heal by waiting, so `--wait` changes nothing
/// there: a healthy file migrates at once, with no progress line.
#[test]
fn migrate_wait_is_a_no_op_on_sqlite() {
    let (url, path) = temp_db_url();
    let started = std::time::Instant::now();
    let (ok, out) = run_migrate(&url, &["--wait", "30s"]);
    assert!(ok, "{out}");
    assert!(out.contains("Migrations applied successfully"), "{out}");
    assert!(!out.contains("waiting for"), "{out}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_connectivity_accepts_wait() {
    let (url, path) = temp_db_url();
    let out = Command::new(orion_bin())
        .args(["test-connectivity", "--wait", "5s"])
        .env("ORION_STORAGE__URL", &url)
        .output()
        .expect("invoke orion-server test-connectivity");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("storage:         OK"), "{stdout}");
    let _ = std::fs::remove_file(&path);

    let out = Command::new(orion_bin())
        .args(["test-connectivity", "--wait", "soon"])
        .output()
        .expect("invoke orion-server test-connectivity");
    assert!(!out.status.success(), "an unparseable duration is refused");
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
        spec["paths"]["/api/v2/data/{channel}"].is_object(),
        "the data plane must be documented"
    );
}
