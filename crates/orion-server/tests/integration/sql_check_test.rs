//! `orion-server sql check` against real PostgreSQL and MySQL servers.
//! Container-gated (`#[ignore]`): run with `just test-containers`.
//!
//! What only a server can show: a statement the owner could prepare that
//! the connector's own role may not run (PostgreSQL 16's
//! `EXPLAIN (GENERIC_PLAN)`), a write checked without being executed, and a
//! `--schema` built and rolled back so nothing persists.

use std::process::Command;

use sqlx::Connection;
use testcontainers::core::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::postgres::Postgres;

use crate::common::{ScratchDir, orion_bin};

/// A set whose one connector `name` dials `url`, and whose workflow runs
/// `statements` — `(task id, function, query, params)` — on it.
fn set(
    label: &str,
    name: &str,
    url: &str,
    statements: &[(&str, &str, &str, serde_json::Value)],
) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let defs = scratch.path().join("defs");
    std::fs::create_dir_all(&defs).unwrap();
    std::fs::write(
        defs.join("connector.json"),
        serde_json::json!({"name": name, "connector_type": "db", "config": {
            "type": "db", "connection_string": url, "allow_private_urls": true}})
        .to_string(),
    )
    .unwrap();
    let tasks: Vec<serde_json::Value> = statements
        .iter()
        .map(|(id, function, query, params)| {
            let mut input = serde_json::json!({"connector": name, "query": query, "params": params});
            if *function == "db_read" {
                input["output"] = serde_json::json!(format!("data.{id}"));
            }
            serde_json::json!({"id": id, "name": id, "function": {"name": function, "input": input}})
        })
        .collect();
    std::fs::write(
        defs.join("w.json"),
        serde_json::json!({"workflow_id": "w", "name": "W", "tasks": tasks}).to_string(),
    )
    .unwrap();
    scratch
}

fn sql_check(scratch: &ScratchDir, extra: &[&str]) -> (i32, String, String) {
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

async fn exec(url: &str, sql: &str) {
    let mut conn = sqlx::PgConnection::connect(url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut conn)
        .await
        .unwrap();
}

const PG_SETUP: &str = "\
    CREATE TABLE orders (id int PRIMARY KEY, total int NOT NULL, secret text);
    INSERT INTO orders VALUES (1, 10, 's');
    CREATE ROLE gate LOGIN PASSWORD 'gate';
    GRANT SELECT (id, total) ON orders TO gate;
    GRANT UPDATE (total) ON orders TO gate;";

fn pg_statements() -> Vec<(&'static str, &'static str, &'static str, serde_json::Value)> {
    vec![
        (
            "ok",
            "db_read",
            "SELECT total FROM orders WHERE id = $1",
            serde_json::json!([1]),
        ),
        (
            "grant",
            "db_read",
            "SELECT secret FROM orders",
            serde_json::json!([]),
        ),
        (
            "column",
            "db_read",
            "SELECT nope FROM orders",
            serde_json::json!([]),
        ),
        (
            "write",
            "db_write",
            "UPDATE orders SET total = total + 1 WHERE id = $1",
            serde_json::json!([1]),
        ),
        (
            "count",
            "db_read",
            "SELECT total FROM orders WHERE id = $1 AND total > $2",
            serde_json::json!([1]),
        ),
        (
            "gap",
            "db_read",
            "SELECT total FROM orders WHERE id = $2",
            serde_json::json!([1, 2]),
        ),
    ]
}

/// PostgreSQL 16: the connector's own role is held to its grants, a write
/// is checked and not executed, and every failure is reported.
#[tokio::test]
#[ignore]
async fn postgres_16_proves_the_connectors_grants() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let owner = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    exec(&owner, PG_SETUP).await;
    let gate = format!("postgres://gate:gate@127.0.0.1:{port}/postgres");

    let scratch = set("sql_check_pg16", "orders-gate", &gate, &pg_statements());
    let (code, stdout, stderr) = sql_check(&scratch, &[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("grants proven"), "{stdout}");
    assert!(stdout.contains("role gate"), "{stdout}");
    assert!(
        stderr.contains("task 'grant'") && stderr.contains("SQLSTATE 42501"),
        "the ungranted column is refused for the connector's role: {stderr}"
    );
    assert!(stderr.contains("SQLSTATE 42703"), "{stderr}");
    assert!(stderr.contains("error: [sql.params]"), "{stderr}");
    assert!(
        stderr.contains("warning: [sql.check]") && stderr.contains("value-shaped binding"),
        "a skipped $n is the runtime's fallback, not a failure: {stderr}"
    );
    assert!(
        !stderr.contains("task 'write'"),
        "the granted UPDATE passes: {stderr}"
    );

    // Checked, not executed.
    let mut conn = sqlx::PgConnection::connect(&owner).await.unwrap();
    let total: i32 = sqlx::query_scalar("SELECT total FROM orders WHERE id = 1")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(total, 10);

    // As the owner the same statement passes: an owner-only PREPARE never
    // proves the role's grants.
    let scratch = set(
        "sql_check_pg16_owner",
        "orders-owner",
        &owner,
        &pg_statements()[..2],
    );
    let (code, stdout, stderr) = sql_check(&scratch, &[]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// Before 16 the grants cannot be proven: the schema is, and the report
/// says which.
#[tokio::test]
#[ignore]
async fn postgres_before_16_proves_the_schema_and_says_so() {
    let container = Postgres::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let owner = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    exec(&owner, PG_SETUP).await;
    let gate = format!("postgres://gate:gate@127.0.0.1:{port}/postgres");
    let scratch = set(
        "sql_check_pg_old",
        "orders-gate",
        &gate,
        &pg_statements()[..3],
    );
    let (code, stdout, stderr) = sql_check(&scratch, &[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("grants not proven"), "{stdout}");
    assert!(stderr.contains("note: [sql.proof]"), "{stderr}");
    assert!(stderr.contains("SQLSTATE 42703"), "{stderr}");
    assert!(
        !stderr.contains("42501"),
        "grants are not checked before 16: {stderr}"
    );
}

/// `--schema`: the migrations are applied in one transaction, every
/// statement checked as the connector's role, and all of it rolled back.
#[tokio::test]
#[ignore]
async fn a_scratch_schema_is_checked_as_the_role_and_nothing_persists() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let owner = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let scratch = set(
        "sql_check_pg_schema",
        "items-db",
        "env://ORION_SQL_CHECK_TEST_NEVER_SET",
        &[
            (
                "id",
                "db_read",
                "SELECT id FROM items",
                serde_json::json!([]),
            ),
            (
                "name",
                "db_read",
                "SELECT name FROM items",
                serde_json::json!([]),
            ),
        ],
    );
    let migrations = scratch.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    std::fs::write(
        migrations.join("001_items.sql"),
        "CREATE TABLE items (id int PRIMARY KEY, name text);\nCREATE ROLE reader;\n\
         GRANT SELECT (id) ON items TO reader;",
    )
    .unwrap();
    let dir = migrations.to_str().unwrap();
    let (code, stdout, stderr) = sql_check(
        &scratch,
        &[
            "--schema",
            dir,
            "--database",
            &owner,
            "--role",
            "items-db=reader",
        ],
    );
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stderr.contains("task 'name'") && stderr.contains("42501"),
        "{stderr}"
    );
    assert!(!stderr.contains("task 'id'"), "{stderr}");

    let mut conn = sqlx::PgConnection::connect(&owner).await.unwrap();
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('items')::text")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(table, None, "the scratch table was rolled back");
    let roles: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname = 'reader'")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(roles, 0, "the scratch role was rolled back");

    // A migration that would end the transaction is refused before anything
    // is sent.
    std::fs::write(migrations.join("002_commit.sql"), "COMMIT;").unwrap();
    let (code, _, stderr) = sql_check(
        &scratch,
        &[
            "--schema",
            dir,
            "--database",
            &owner,
            "--role",
            "items-db=reader",
        ],
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("002_commit.sql") && stderr.contains("ends or controls the transaction"),
        "{stderr}"
    );
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('items')::text")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(table, None, "nothing was sent");
}

/// MySQL prepares — schema proven, grants not — and refuses `--schema`,
/// whose DDL it would commit.
#[tokio::test]
#[ignore]
async fn mysql_prepares_and_refuses_a_scratch_schema() {
    let container = Mysql::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(3306).await.unwrap();
    let url = format!("mysql://root@127.0.0.1:{port}/test");
    let mut conn = sqlx::MySqlConnection::connect(&url).await.unwrap();
    sqlx::raw_sql("CREATE TABLE orders (id int PRIMARY KEY, total int)")
        .execute(&mut conn)
        .await
        .unwrap();
    let scratch = set(
        "sql_check_mysql",
        "orders-my",
        &url,
        &[
            (
                "ok",
                "db_read",
                "SELECT total FROM orders WHERE id = ?",
                serde_json::json!([1]),
            ),
            (
                "column",
                "db_read",
                "SELECT nope FROM orders",
                serde_json::json!([]),
            ),
            (
                "count",
                "db_read",
                "SELECT total FROM orders WHERE id = ?",
                serde_json::json!([]),
            ),
        ],
    );
    let (code, stdout, stderr) = sql_check(&scratch, &[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("1 ok, 2 failed") && stdout.contains("grants not proven"),
        "{stdout}"
    );
    assert!(stderr.contains("task 'column'"), "{stderr}");
    assert!(stderr.contains("error: [sql.params]"), "{stderr}");

    let migrations = scratch.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    std::fs::write(migrations.join("001.sql"), "CREATE TABLE x (id int);").unwrap();
    let (code, _, stderr) = sql_check(
        &scratch,
        &["--schema", migrations.to_str().unwrap(), "--database", &url],
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("MySQL commits DDL implicitly"), "{stderr}");
}
