//! `orion-server sql check` against SQLite, which needs no container: every
//! statement is prepared, every failure is reported rather than the first,
//! and the database file is never written.

use std::path::Path;
use std::process::Command;

use crate::common::{ScratchDir, orion_bin};

const SCHEMA: &str = "CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER NOT NULL, \
                      status TEXT);";

/// A set with one connector on a SQLite file holding `SCHEMA`, and a
/// workflow whose tasks are `tasks`.
fn set(label: &str, tasks: serde_json::Value) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let db = scratch.path().join("orders.db");
    let migrations = scratch.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    std::fs::write(migrations.join("001_orders.sql"), SCHEMA).unwrap();
    let defs = scratch.path().join("defs");
    std::fs::create_dir_all(&defs).unwrap();
    std::fs::write(
        defs.join("connector.json"),
        serde_json::json!({"name": "orders-db", "connector_type": "db",
            "config": {"type": "db", "connection_string": format!("sqlite:{}", db.display())}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        defs.join("w.json"),
        serde_json::json!({"workflow_id": "orders", "name": "Orders", "tasks": tasks}).to_string(),
    )
    .unwrap();
    create_database(&db);
    scratch
}

/// Create the SQLite file with the schema, through sqlx on a runtime of its
/// own (the test is synchronous; the binary under test opens the file
/// read-only).
fn create_database(db: &Path) {
    let url = format!("sqlite:{}?mode=rwc", db.display());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use sqlx::Connection;
        let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
        sqlx::raw_sql(SCHEMA).execute(&mut conn).await.unwrap();
    });
}

fn read(id: &str, query: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id": id, "name": id, "function": {"name": "db_read", "input": {
        "connector": "orders-db", "query": query, "params": params, "output": format!("data.{id}")}}})
}

fn check(scratch: &ScratchDir, extra: &[&str]) -> (i32, String, String) {
    let out = Command::new(orion_bin())
        .args(["sql", "check"])
        .arg(scratch.path().join("defs"))
        .args(extra)
        .output()
        .expect("run sql check");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn every_statement_is_prepared_and_every_failure_reported() {
    let scratch = set(
        "sql_check_failures",
        serde_json::json!([
            read("ok", "SELECT total FROM orders WHERE id = ?", serde_json::json!([1])),
            {"id": "grp", "condition": true, "tasks": [
                read("col", "SELECT missing FROM orders", serde_json::json!([])),
                {"id": "tbl", "name": "tbl", "function": {"name": "db_write", "input": {
                    "connector": "orders-db", "query": "UPDATE nope SET x = 1"}}}
            ]}
        ]),
    );
    let db = scratch.path().join("orders.db");
    let before = std::fs::read(&db).unwrap();
    let (code, stdout, stderr) = check(&scratch, &[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("1 ok, 2 failed"), "{stdout}");
    assert!(
        stderr.contains("task 'col'") && stderr.contains("no such column: missing"),
        "{stderr}"
    );
    assert!(
        stderr.contains("task 'tbl'") && stderr.contains("no such table: nope"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tasks[1].tasks[0].function.input.query"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "the database is never written"
    );
}

#[test]
fn a_clean_set_passes_and_a_count_mismatch_is_a_warning_on_sqlite() {
    let scratch = set(
        "sql_check_clean",
        serde_json::json!([
            read(
                "one",
                "SELECT total FROM orders WHERE id = ?",
                serde_json::json!([1])
            ),
            read(
                "short",
                "SELECT total FROM orders WHERE id = ? AND total > ?",
                serde_json::json!([1])
            )
        ]),
    );
    let (code, stdout, stderr) = check(&scratch, &[]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.contains("warning: [sql.params]"), "{stderr}");
    assert!(stderr.contains("binds a missing value as NULL"), "{stderr}");
}

#[test]
fn a_connector_outside_the_set_must_be_named_or_skipped() {
    let scratch = set(
        "sql_check_outside",
        serde_json::json!([{"id": "far", "name": "far", "function": {"name": "db_read",
            "input": {"connector": "elsewhere", "query": "SELECT 1", "output": "data.x"}}}]),
    );
    let (code, _, stderr) = check(&scratch, &["--requires-connector", "elsewhere"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "connector 'elsewhere' is not in this set — pass --connector elsewhere=<url>"
        ),
        "{stderr}"
    );
    let (code, stdout, stderr) = check(
        &scratch,
        &[
            "--requires-connector",
            "elsewhere",
            "--skip-connector",
            "elsewhere",
        ],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.contains("warning: [sql.unchecked]"), "{stderr}");
    let db = scratch.path().join("orders.db");
    let url = format!("elsewhere=sqlite:{}", db.display());
    let (code, stdout, stderr) = check(
        &scratch,
        &["--requires-connector", "elsewhere", "--connector", &url],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
}

#[test]
fn a_scratch_schema_is_built_in_memory_and_json_is_one_object_per_line() {
    let scratch = set(
        "sql_check_schema",
        serde_json::json!([read(
            "col",
            "SELECT status FROM orders",
            serde_json::json!([])
        )]),
    );
    // Point the connector at a file that does not exist: `--schema` never
    // opens it.
    std::fs::remove_file(scratch.path().join("orders.db")).unwrap();
    let migrations = scratch.path().join("migrations");
    let (code, stdout, stderr) = check(
        &scratch,
        &["--schema", migrations.to_str().unwrap(), "--format", "json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let last: serde_json::Value =
        serde_json::from_str(stdout.lines().last().expect("summary")).expect("json");
    assert_eq!(last["summary"]["statements"], 1);
    assert_eq!(last["summary"]["connectors"][0]["ok"], 1);
    assert!(
        !scratch.path().join("orders.db").exists(),
        "nothing was created"
    );
}

#[test]
fn lint_errors_stop_the_check_before_any_connection() {
    let scratch = set(
        "sql_check_lint",
        serde_json::json!([{"id": "bad", "function": {"name": "db_read",
            "input": {"connector": "orders-db", "query": "SELECT 1"}}}]),
    );
    let (code, stdout, _) = check(&scratch, &[]);
    assert_eq!(code, 2);
    assert!(stdout.contains("no statement was checked"), "{stdout}");
}
