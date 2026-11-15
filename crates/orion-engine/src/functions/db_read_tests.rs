use super::*;

fn check(sql: &str) -> Result<(), String> {
    require_read_only(sql, "db_read").map_err(|e| {
        let e: dataflow_rs::Error = e.into();
        e.to_string()
    })
}

#[test]
fn reads_are_admitted() {
    for sql in [
        "SELECT id FROM users WHERE id = $1",
        "  \n select 1",
        "-- a comment\nSELECT 1",
        "/* block */ SELECT 1",
        "(SELECT 1) UNION (SELECT 2)",
        "WITH recent AS (SELECT * FROM orders) SELECT * FROM recent",
        "VALUES (1), (2)",
        "TABLE users",
        // A locking read is a read: `FOR UPDATE` must not be mistaken for
        // an UPDATE statement.
        "SELECT id FROM jobs ORDER BY id FOR UPDATE SKIP LOCKED",
        // The word only appears inside data.
        "SELECT id FROM notes WHERE body = 'delete from users'",
        "SELECT \"delete\" FROM t",
        "SELECT total AS deleted FROM t",
        "SELECT CAST(a AS text) FROM t",
    ] {
        assert!(
            check(sql).is_ok(),
            "must be admitted: {sql} — {:?}",
            check(sql)
        );
    }
}

#[test]
fn writes_are_refused() {
    for sql in [
        "DELETE FROM audit_log WHERE id > 0 RETURNING id",
        "delete from audit_log",
        "INSERT INTO t (a) VALUES (1)",
        "UPDATE t SET a = 1",
        "TRUNCATE t",
        "DROP TABLE t",
        "PRAGMA journal_mode = WAL",
        // `EXPLAIN ANALYZE` executes the statement it explains.
        "EXPLAIN ANALYZE DELETE FROM t",
        "  -- lead in\n  DELETE FROM t",
    ] {
        let err = check(sql).expect_err(&format!("must be refused: {sql}"));
        assert!(err.contains("read statements only"), "{sql}: {err}");
    }
}

#[test]
fn a_data_modifying_cte_is_refused() {
    for sql in [
        "WITH gone AS (DELETE FROM t RETURNING id) SELECT * FROM gone",
        "WITH added AS (INSERT INTO t (a) VALUES (1) RETURNING id) SELECT * FROM added",
        "with m as materialized (update t set a = 1 returning id) select * from m",
    ] {
        let err = check(sql).expect_err(&format!("must be refused: {sql}"));
        assert!(err.contains("data-modifying"), "{sql}: {err}");
    }
}

/// A statement whose text merely *mentions* a modifying keyword inside a
/// literal, a comment or an identifier stays a read — the check reads
/// syntax, not data.
#[test]
fn quoted_text_is_not_syntax() {
    assert!(check("SELECT 1 /* AS (DELETE */").is_ok());
    assert!(check("SELECT 'x AS (DELETE FROM t)' AS s").is_ok());
    assert!(check("SELECT $tag$ AS (DELETE FROM t) $tag$ AS s").is_ok());
    assert!(check("SELECT * FROM t WHERE a = $1 AND b = $2").is_ok());
}

#[test]
fn an_empty_statement_is_refused() {
    let err = check("   -- nothing here\n").expect_err("empty");
    assert!(err.contains("no statement"), "{err}");
}

// -- the config, I185's Defect, and calls over in-memory SQLite, against
// 1.12's records --

use std::time::Duration;

use crate::functions::twin::{self, Seen};
use dataflow_rs::ErrorKind;
use dataflow_rs::function::testing;
use dataflow_rs::program::{IssueCode, Severity};
use serde_json::json;

/// Three rows: a decimal-shaped column, which SQLite stores as whichever
/// class fits, and a blob that is not text, one that is, and a `NULL`.
pub(crate) const SEED: &[&str] = &[
    "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price NUMERIC, blob BLOB)",
    "INSERT INTO items VALUES (1, 'apple', 1.25, X'00FF'), (2, 'pear', 2, X'6869'), \
     (3, 'plum', 3.5, NULL)",
];

fn function(db: &twin::Sqlite, max_rows: usize) -> DbReadFunction {
    DbReadFunction {
        pool_cache: db.pool_cache.clone(),
        registry: db.registry.clone(),
        max_rows,
        max_result_bytes: 0,
    }
}

/// One task against a database of its own seeded with [`SEED`], its
/// connector configured with `config`.
async fn run(input: Value, data: Value, config: Value, max_rows: usize) -> Seen {
    let db = twin::sqlite(config, SEED).await;
    twin::v4(NAME, function(&db, max_rows), input, data).await
}

fn rejected(input: Value) -> Vec<(String, IssueCode)> {
    testing::config_with::<DbReadConfig>(&kit::test_exprs(), input)
        .expect_err("refused")
        .into_iter()
        .filter(|i| i.severity == Severity::Rejected)
        .map(|i| (i.path.to_string(), i.code))
        .collect()
}

/// Declaration order is the field table's (I179). `params`,
/// `numeric_as` and `binary_as` are documents of `var` holes, a `$` key
/// kept as data; `connector` and `query` are literals, required (I183);
/// an unknown key is refused (break 2a); `output` defaults to `data`.
#[test]
fn the_config_compiles_and_checks_its_inputs() {
    let params = json!([
        {"var": "data.id"},
        {"$oid": {"var": ["data.oid", "none"]}},
        "literal"
    ]);
    let cfg = testing::config_with::<DbReadConfig>(
        &kit::test_exprs(),
        json!({
            "connector": "db", "query": "SELECT * FROM t WHERE id = ?",
            "params": params.clone(), "numeric_as": {"var": "data.num"},
            "binary_as": "hex"
        }),
    )
    .expect("compiles");
    let doc = cfg.params.as_ref().expect("set");
    assert_eq!(
        doc.raw(),
        &params,
        "the document as authored, `$` keys kept"
    );
    let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
    assert_eq!(holes, ["data.id", "data.oid"]);
    assert_eq!(cfg.query, "SELECT * FROM t WHERE id = ?");
    let output = cfg.output.constant().map(ToString::to_string);
    assert_eq!(output.as_deref(), Some("data"));

    // A literal that is not an array, as 1.12 had it, is refused by the
    // call, not at load.
    testing::config_with::<DbReadConfig>(
        &kit::test_exprs(),
        json!({"connector": "db", "query": "SELECT 1", "params": "x"}),
    )
    .expect("compiles");

    assert_eq!(
        rejected(json!({"connector": "db"})),
        [("query".to_string(), IssueCode::InputParse)]
    );
    assert_eq!(
        rejected(json!({"query": "SELECT 1"})),
        [("connector".to_string(), IssueCode::InputParse)]
    );
    assert_eq!(
        rejected(json!({"connector": {"var": "data.c"}, "query": "SELECT 1"})),
        [("connector".to_string(), IssueCode::InputParse)]
    );
    assert_eq!(
        rejected(json!({"connector": "db", "query": {"cat": ["SELECT ", "1"]}})),
        [("query".to_string(), IssueCode::InputParse)]
    );
    let issues = rejected(json!({"connector": "db", "query": "SELECT 1", "database": "d"}));
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].0, "database");
}

/// I185: the rows are never an object, so a literal root `output`, the
/// default `data` included, is a Defect at `output`, reported by
/// `prepare`, which only a compile through the registry runs; a path
/// below a root is not. The call still runs, and the root's write is
/// refused (break 21), where 1.12 replaced the root with the array.
#[tokio::test]
async fn a_root_output_is_i185s_defect() {
    for (output, defect) in [
        (None, true),
        (Some(json!("data")), true),
        (Some(json!("temp_data")), true),
        (Some(json!(null)), true),
        (Some(json!("data.rows")), false),
        (Some(json!({"cat": ["data", ".rows"]})), false),
    ] {
        let db = twin::sqlite(json!({}), SEED).await;
        let mut input = json!({"connector": "db", "query": "SELECT id FROM items"});
        if let Some(output) = &output {
            input["output"] = output.clone();
        }
        let called = testing::call(function(&db, 1000), input)
            .exprs(kit::test_exprs())
            .name(NAME)
            .await;
        let defects: Vec<_> = called
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Defect)
            .map(|i| (i.path.to_string(), i.code, i.message.as_str()))
            .collect();
        if defect {
            assert_eq!(
                defects,
                [("output".to_string(), IssueCode::InputParse, ROOT_OUTPUT)],
                "{output:?}"
            );
            let error = called.outcome.expect_err("an array at a root is refused");
            assert_eq!(error.code(), "CONFIG_ERROR", "{output:?}");
        } else {
            assert!(defects.is_empty(), "{output:?}: {defects:?}");
            assert_eq!(
                called.outcome.as_ref().ok(),
                Some(&Outcome::Ok),
                "{output:?}"
            );
            let rows = twin::seen(&called).expect("read")["rows"].clone();
            assert_eq!(rows, json!([{"id": 1}, {"id": 2}, {"id": 3}]), "{output:?}");
        }
    }
}

/// The rows are written at `output`, the binds filled from the message.
#[tokio::test]
async fn rows_are_written_at_output() {
    let input = json!({
        "connector": "db",
        "query": "SELECT id, name FROM items WHERE id >= ? AND name <> ? ORDER BY id",
        "params": [{"var": "data.min"}, {"var": ["data.skip", "none"]}],
        "output": "data.rows"
    });
    let v4 = run(input, json!({"min": 2}), json!({}), 1000).await;
    twin::pinned(&v4, "rows_are_written_at_output");
    assert_eq!(
        v4.expect("read")["rows"],
        json!([{"id": 2, "name": "pear"}, {"id": 3, "name": "plum"}])
    );
}

/// A bound document keeps its `$` keys and has its holes filled, and a
/// `payload` hole yields its default, as 1.12's fold did.
#[tokio::test]
async fn a_bound_document_keeps_its_dollar_keys() {
    let input = json!({
        "connector": "db",
        "query": "SELECT ? AS doc, ? AS missing, ? AS payload",
        "params": [
            {"$oid": {"var": "data.id"}, "n": [1, {"var": "data.n"}]},
            {"var": "data.missing"},
            {"var": ["payload.x", "fallback"]}
        ],
        "output": "data.rows"
    });
    let v4 = run(input, json!({"id": "abc", "n": 2}), json!({}), 1000).await;
    twin::pinned(&v4, "a_bound_document_keeps_its_dollar_keys");
    assert_eq!(
        v4.expect("read")["rows"],
        json!([{"doc": r#"{"$oid":"abc","n":[1,2]}"#, "missing": null, "payload": "fallback"}])
    );
}

/// `numeric_as` and `binary_as` fold their holes and are read as 1.12
/// read them: a rendering named, the default for `null` or a hole that
/// finds nothing, and 1.12's refusal for a name or a type it refused.
#[tokio::test]
async fn numeric_as_and_binary_as_fold_as_1_12_did() {
    let data = json!({"num": "string", "hex": "hex", "b64": "base64", "bad": "money", "n": 7});
    for (numeric, binary, ok) in [
        (json!({"var": "data.num"}), json!({"var": "data.hex"}), true),
        (json!("number"), json!({"var": "data.b64"}), true),
        (json!(null), json!(null), true),
        (
            json!({"var": "data.none"}),
            json!({"var": "data.none"}),
            true,
        ),
        (json!({"var": "data.bad"}), json!("hex"), false),
        (json!("string"), json!({"var": "data.n"}), false),
        (json!({"cat": ["str", "ing"]}), json!("hex"), false),
    ] {
        let input = json!({
            "connector": "db",
            "query": "SELECT id, price, blob FROM items ORDER BY id",
            "numeric_as": numeric, "binary_as": binary, "output": "data.rows"
        });
        let v4 = run(input, data.clone(), json!({}), 1000).await;
        twin::pinned(&v4, format!("numeric_as_and_binary_as/{numeric} {binary}"));
        assert_eq!(v4.is_ok(), ok, "{numeric} {binary}: {v4:?}");
    }
    // `hex` is honoured: the second row's `hi` is its bytes, not text.
    let input = json!({
        "connector": "db", "query": "SELECT blob FROM items WHERE id = 2",
        "binary_as": {"var": "data.hex"}, "output": "data.rows"
    });
    let v4 = run(input, data, json!({}), 1000).await;
    assert_eq!(v4.expect("read")["rows"], json!([{"blob": "6869"}]));
}

/// Every refusal on the way: the statement, the binds, the gates, the
/// connector, the row cap, the database's own, and an output that names
/// no path; each with 1.12's code and text.
#[tokio::test]
async fn refusals_are_1_12s() {
    let select = json!({"connector": "db", "query": "SELECT id FROM items", "output": "data.rows"});
    let with = |key: &str, value: Value| {
        let mut input = select.clone();
        input[key] = value;
        input
    };
    let data = json!({"obj": {"k": 1}, "n": 3});
    for (input, config, max_rows) in [
        (with("query", json!("DELETE FROM items")), json!({}), 1000),
        (
            with(
                "query",
                json!("WITH gone AS (DELETE FROM items RETURNING id) SELECT * FROM gone"),
            ),
            json!({}),
            1000,
        ),
        (with("params", json!({"var": "data.obj"})), json!({}), 1000),
        (with("params", json!("x")), json!({}), 1000),
        (select.clone(), json!({"operations": {"read": false}}), 1000),
        (with("connector", json!("absent")), json!({}), 1000),
        (select.clone(), json!({}), 2),
        (
            with("query", json!("SELECT nope FROM items")),
            json!({}),
            1000,
        ),
        (with("output", json!({"var": "data.n"})), json!({}), 1000),
    ] {
        let key = format!("refusals/{input} {config} {max_rows}");
        let v4 = run(input.clone(), data.clone(), config, max_rows).await;
        twin::pinned(&v4, key);
        assert!(v4.is_err(), "{input}");
    }
}

/// The connector's kind, as 1.12 refused it: not a `db` connector.
#[tokio::test]
async fn a_connector_of_another_kind_or_backend_is_refused() {
    for connector in [json!({"type": "http", "url": "http://127.0.0.1:1"})] {
        let input = json!({"connector": "db", "query": "SELECT 1", "output": "data.rows"});
        let pool_cache = Arc::new(SqlPoolCache::new(8));
        let registry = twin::connectors(&[("db", connector.clone())]).await;
        let v4 = twin::v4(
            NAME,
            DbReadFunction {
                pool_cache,
                registry,
                max_rows: 1000,
                max_result_bytes: 0,
            },
            input,
            Value::Null,
        )
        .await;
        twin::pinned(
            &v4,
            format!("a_connector_of_another_kind_or_backend/{connector}"),
        );
        assert!(v4.is_err(), "{connector}");
    }
}

fn select_rows() -> Value {
    json!({"connector": "db", "query": "SELECT id FROM items", "output": "data.rows"})
}

/// `QueryBudget` within the run's deadline (E10), the one connection held
/// so the read waits on the pool: a run deadline before the connector's
/// `query_timeout_ms` is the run's, and ends the call `TimedOut` at the
/// deadline, as 1.12's channel timeout ended the run, writing nothing.
#[tokio::test]
async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
    let db = twin::sqlite(json!({"query_timeout_ms": 30_000}), SEED).await;
    let _held = db.pool.acquire().await.expect("the one connection");
    let started = std::time::Instant::now();
    let called = testing::call(function(&db, 1000), select_rows())
        .exprs(kit::test_exprs())
        .name(NAME)
        .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
        .await;
    let error = called.outcome.expect_err("the run's deadline passed");
    assert_eq!(*error.kind(), ErrorKind::TimedOut);
    assert_eq!(
        error.message(),
        "the run's deadline passed during `db_read`"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(called.writes.is_empty());
}

/// A `query_timeout_ms` before the run's deadline is the budget's, with
/// 1.12's code, text and retryability.
#[tokio::test]
async fn a_query_timeout_before_the_run_deadline_is_1_12s() {
    let db = twin::sqlite(json!({"query_timeout_ms": 150}), SEED).await;
    let _held = db.pool.acquire().await.expect("the one connection");
    let called = testing::call(function(&db, 1000), select_rows())
        .exprs(kit::test_exprs())
        .name(NAME)
        .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
        .await;
    let v4 = twin::seen(&called);
    twin::pinned(&v4, "a_query_timeout_before_the_run_deadline");
    let failure = v4.expect_err("the query timed out");
    assert_eq!(failure.code, "TIMEOUT_ERROR");
    assert_eq!(
        failure.message,
        "Timeout error: db_read query timed out after 150ms"
    );
    assert!(failure.retryable);
}

#[tokio::test]
async fn result_byte_cap_refuses_oversized_rows_as_400_limit_error() {
    let input = json!({
        "connector": "db",
        "query": "SELECT id, name, price, blob FROM items ORDER BY id",
        "output": "data.rows"
    });
    let db4 = twin::sqlite(json!({}), SEED).await;
    let v4 = twin::v4(
        NAME,
        DbReadFunction {
            pool_cache: db4.pool_cache,
            registry: db4.registry,
            max_rows: 1000,
            max_result_bytes: 64,
        },
        input,
        Value::Null,
    )
    .await;
    twin::pinned(&v4, "td09_result_byte_cap");
    let failure = v4.expect_err("oversized result set must be refused");
    assert_eq!(failure.code, "VALIDATION_ERROR");
    assert!(
        failure
            .message
            .contains("query.max_result_bytes (64 bytes)"),
        "expected max_result_bytes diagnostic, got: {}",
        failure.message
    );
    assert!(!failure.retryable);
}
