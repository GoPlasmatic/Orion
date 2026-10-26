use super::definition::{Definition, FieldOverlay, OrionFunction};
use std::sync::Arc;

use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Function, Outcome};

use super::connector_helpers::{
    QueryBudget, QueryFailure, acquire_conn, encode_failure, require_op_allowed, to_connect_error,
};
use super::db_read::DbRead;
use super::schema::{FieldKind, RetrySafety};
use crate::HandlerError;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::kit;

/// This handler's name, for the shared write below (F48).
pub(crate) const NAME: &str = "db_write";

/// The connector's gate for a raw write: its `raw_write` operation. Shared by
/// the dataflow-rs 3 handler and the dataflow-rs 4 function.
fn gate(conn: &DbConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    // Raw SQL cannot be classified per-op; it has its own gate.
    require_op_allowed(&conn.operations, "raw_write", connector)
}

/// The write's answer, `{"rows_affected": n}` and, for an insert on a driver
/// that reports one, `"last_insert_id"`, against a resolved and gated
/// connector. Shared by the dataflow-rs 3 handler and the dataflow-rs 4
/// function. `run_deadline` is the deadline of the run the call belongs to
/// ([`QueryBudget::within`]).
async fn write_rows(
    pool_cache: &SqlPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    write: &DbRead<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<serde_json::Value, HandlerError> {
    let pool = pool_cache
        .get_pool(connector, db_config)
        .await
        .map_err(to_connect_error)?;

    let query = write.query();
    let params = write.params();
    // One budget over both legs, so naming the connection does not turn
    // the connector's `query_timeout_ms` into `connect_timeout_ms` plus it.
    let budget = QueryBudget::within(db_config.query_timeout_ms, run_deadline);
    let scalars: Vec<crate::connector::sql_encode::Scalar> =
        params.iter().map(Into::into).collect();
    let (rows_affected, last_insert_id) = crate::connector::pool_cache::dispatch_sql_pool!(
        &pool, p, _decode, bind, typed_args, write_result => {
            // Named for the same reason as `db_read`'s: a prepared
            // statement's parameter types are cached per connection, so
            // the prepare and the execute have to share one.
            let mut conn = acquire_conn(&budget, NAME, p).await?;
            let bound = budget
                .run(NAME, async {
                    typed_args(&mut conn, query, Some(&scalars))
                        .await
                        .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                })
                .await?;
                // `AssertSqlSafe` states what this handler is: the
                // raw-SQL escape hatch, whose statement is authored in the
                // workflow rather than assembled here. sqlx 0.9 asks the
                // caller to own that, and the answer is the same as it was
                // before it asked — the text comes from the definition, the
                // author-supplied *values* travel as bind parameters beside
                // it, and a definition reaches the runtime only through the
                // admin API. Authors who want values checked use the
                // portable dialect (`data_query`/`data_write`) instead.
            let sqlx_query = match bound {
                crate::connector::sql_encode::Bound::Typed(args) => {
                    sqlx::query_with(sqlx::AssertSqlSafe(query), args)
                }
                crate::connector::sql_encode::Bound::Fallback { cache } => {
                    bind(sqlx::query(sqlx::AssertSqlSafe(query)), params).persistent(cache)
                }
            };
            let result = budget
                .run(NAME, sqlx_query.execute(&mut *conn))
                .await?;
            write_result(&result)
        }
    );

    let mut out = serde_json::json!({ "rows_affected": rows_affected });
    // The generated key, on the drivers that report one — the same answer
    // `data_write` gives, so reaching for the escape hatch does not cost
    // the most common follow-up question after an insert.
    //
    // Only for an insert, and that is not pedantry: SQLite's
    // `last_insert_rowid` is a property of the *connection*, so after an
    // UPDATE it reports whatever an earlier insert on that pooled
    // connection left behind. Raw SQL cannot be classified per-op — which
    // is why `db_write` has its own `raw_write` gate — but the leading
    // keyword is enough to know whether an id was generated at all.
    if let Some(id) = last_insert_id
        && matches!(
            orion_definitions::sql_lex::leading_keyword(query).as_deref(),
            Some("INSERT" | "REPLACE")
        )
    {
        out["last_insert_id"] = serde_json::json!(id);
    }

    Ok(out)
}

/// `db_write`'s input on dataflow-rs 4, in its field table's order (I179):
/// the connector and the statement literals required at load (I183), the
/// binds a document of `var` holes, as 1.12 folded them.
#[derive(Debug, FunctionConfig)]
pub struct DbWriteConfig {
    /// Name of the SQL connector to execute against.
    connector: String,
    /// INSERT/UPDATE/DELETE statement. Bind placeholders are the backend's own spelling: ? for
    /// SQLite and MySQL, $1, $2, ... for PostgreSQL.
    query: String,
    /// Array of values to bind to query placeholders, in order. Accepts {"var": "path"} to read
    /// the value from the message.
    params: Option<Interpolated>,
    /// Dotted path where the write result is written: {"rows_affected": n}, plus
    /// "last_insert_id" for an INSERT on MySQL or SQLite (PostgreSQL reports generated columns
    /// through RETURNING instead).
    #[output(default = "data")]
    output: Output,
}

/// `db_write` as a dataflow-rs 4 function: the statement `DbWriteHandler`
/// runs, read in the same order.
pub struct DbWriteFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

impl OrionFunction for DbWriteFunction {
    type Connector = crate::connector::kind::Db;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Execute INSERT/UPDATE/DELETE against a SQL connector.",
        category: "connector",
        retry_safety: RetrySafety::DependsOn { input: "sql" },
        fields: &[FieldOverlay {
            name: "params",
            kind: Some(FieldKind::Array),
            resolvable: true,
            ..FieldOverlay::DEFAULT
        }],
        deny_unknown: false,
        stub_target: Some("connector"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            pool_cache: deps.sql_pool_cache.clone(),
            registry: deps.registry.clone(),
        }
    }
}

impl Function for DbWriteFunction {
    type Config = DbWriteConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DbWriteConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
        let write = DbRead::statement(ctx, &cfg.query, cfg.params.as_ref(), NAME)?;
        let run_deadline = ctx.deadline();
        let mut answer = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<<Self as OrionFunction>::Connector>(
                &config,
                &cfg.connector,
            )?;
            gate(db, &cfg.connector)?;
            answer =
                Some(write_rows(&self.pool_cache, &cfg.connector, db, &write, run_deadline).await?);
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(answer) = answer {
            ctx.set(
                &output,
                dataflow_rs::datavalue::OwnedDataValue::from(&answer),
            )?;
        }
        Ok(Outcome::Ok)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::functions::twin::{self, Seen};
    use dataflow_rs::ErrorKind;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::{Value, json};

    const SEED: &[&str] = &[
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
        "INSERT INTO items (name) VALUES ('apple')",
    ];

    fn function(db: &twin::Sqlite) -> DbWriteFunction {
        DbWriteFunction {
            pool_cache: db.pool_cache.clone(),
            registry: db.registry.clone(),
        }
    }

    /// One task against a database of its own seeded with [`SEED`], its
    /// connector configured with `config`; and the database's rows after it.
    async fn run(input: Value, data: Value, config: Value) -> (Seen, Value) {
        let db = twin::sqlite(config, SEED).await;
        let seen = twin::v4(NAME, function(&db), input, data).await;
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM items ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .expect("the table reads");
        (seen, json!(rows))
    }

    /// `seen` and the rows after it, each against 1.12's record under `key`.
    #[track_caller]
    fn pinned(seen: &(Seen, Value), key: &str) {
        twin::pinned(&seen.0, key);
        twin::pinned(&Ok(seen.1.clone()), format!("{key}/rows"));
    }

    /// Declaration order is the field table's (I179); `params` is a document
    /// of `var` holes; `connector` and `query` are literals, required (I183);
    /// a key the table does not declare is refused (break 2a).
    #[test]
    fn the_config_compiles_and_checks_its_inputs() {
        let params = json!([{"var": "data.name"}, {"$k": [1, {"var": "data.n"}]}]);
        let cfg = testing::config_with::<DbWriteConfig>(
            &crate::kit::test_exprs(),
            json!({"connector": "db", "query": "INSERT INTO t VALUES (?, ?)", "params": params.clone()}),
        )
        .expect("compiles");
        let doc = cfg.params.as_ref().expect("set");
        assert_eq!(doc.raw(), &params);
        let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.name", "data.n"]);
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));

        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            testing::config_with::<DbWriteConfig>(&crate::kit::test_exprs(), input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        assert_eq!(
            rejected(json!({"connector": "db"})),
            [("query".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"query": "DELETE FROM t"})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        // `numeric_as` is `db_read`'s: a write decodes no rows.
        let issues =
            rejected(json!({"connector": "db", "query": "DELETE FROM t", "numeric_as": "string"}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "numeric_as");
    }

    /// An insert answers its count and its generated key, at `output`, its
    /// binds from the message; an update answers its count alone; each
    /// database ends the same.
    #[tokio::test]
    async fn a_write_answers_as_1_12_did() {
        for (input, answer) in [
            (
                json!({
                    "connector": "db", "query": "INSERT INTO items (name) VALUES (?)",
                    "params": [{"var": "data.name"}], "output": "data.write"
                }),
                json!({"rows_affected": 1, "last_insert_id": 2}),
            ),
            (
                json!({
                    "connector": "db", "query": "UPDATE items SET name = ? WHERE id = ?",
                    "params": [{"var": "data.name"}, 1], "output": "data.write"
                }),
                json!({"rows_affected": 1}),
            ),
            (
                json!({"connector": "db", "query": "DELETE FROM items WHERE id = 9", "output": "data.write"}),
                json!({"rows_affected": 0}),
            ),
        ] {
            let v4 = run(input.clone(), json!({"name": "pear"}), json!({})).await;
            pinned(&v4, &format!("a_write_answers/{input}"));
            assert_eq!(v4.0.expect("written")["write"], answer, "{input}");
        }
        // With no `output`, the answer replaces `data`, an object a root
        // takes.
        let input = json!({"connector": "db", "query": "DELETE FROM items"});
        let v4 = run(input, json!({"keep": 1}), json!({})).await;
        pinned(&v4, "a_write_answers/no_output");
        assert_eq!(v4.0.expect("written"), json!({"rows_affected": 1}));
    }

    /// The gates, the backend, the binds, the database's integrity refusal
    /// and an output that names no path, each with 1.12's code and text, and
    /// each database untouched.
    #[tokio::test]
    async fn refusals_are_1_12s() {
        let insert = json!({
            "connector": "db", "query": "INSERT INTO items (name) VALUES (?)",
            "params": [{"var": "data.name"}], "output": "data.write"
        });
        let with = |key: &str, value: Value| {
            let mut input = insert.clone();
            input[key] = value;
            input
        };
        for (input, data, config) in [
            (
                insert.clone(),
                json!({"name": "pear"}),
                json!({"operations": {"raw_write": false}}),
            ),
            (insert.clone(), json!({"name": "apple"}), json!({})),
            (
                with("params", json!({"var": "data.name"})),
                json!({"name": "pear"}),
                json!({}),
            ),
            (
                with("connector", json!("absent")),
                json!({"name": "pear"}),
                json!({}),
            ),
            (
                with("output", json!({"var": "data.n"})),
                json!({"name": "pear", "n": 1}),
                json!({}),
            ),
            (
                with("query", json!("INSERT INTO nowhere VALUES (?)")),
                json!({"name": "pear"}),
                json!({}),
            ),
        ] {
            let key = format!("refusals/{input} {data} {config}");
            let v4 = run(input.clone(), data, config).await;
            pinned(&v4, &key);
            assert!(v4.0.is_err(), "{input}");
            assert_eq!(v4.1, json!([[1, "apple"]]), "{input}");
        }
    }

    fn delete_all() -> Value {
        json!({"connector": "db", "query": "DELETE FROM items", "output": "data.write"})
    }

    /// `QueryBudget` within the run's deadline (E10), the one connection held
    /// so the write waits on the pool: a run deadline before the connector's
    /// `query_timeout_ms` ends the call `TimedOut`, as 1.12's channel timeout
    /// ended the run, writing nothing.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        let db = twin::sqlite(json!({"query_timeout_ms": 30_000}), SEED).await;
        let held = db.pool.acquire().await.expect("the one connection");
        let started = std::time::Instant::now();
        let called = testing::call(function(&db), delete_all())
            .exprs(crate::kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
            .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.message(),
            "the run's deadline passed during `db_write`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
        drop(held);
        let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM items")
            .fetch_all(&db.pool)
            .await
            .expect("the table reads");
        assert_eq!(rows, [(1,)], "the statement never ran");
    }

    /// A `query_timeout_ms` before the run's deadline is the budget's, with
    /// 1.12's code, text and retryability.
    #[tokio::test]
    async fn a_query_timeout_before_the_run_deadline_is_1_12s() {
        let db = twin::sqlite(json!({"query_timeout_ms": 150}), SEED).await;
        let _held = db.pool.acquire().await.expect("the one connection");
        let called = testing::call(function(&db), delete_all())
            .exprs(crate::kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
            .await;
        let v4 = twin::seen(&called);
        twin::pinned(&v4, "a_query_timeout_before_the_run_deadline");
        let failure = v4.expect_err("the write timed out");
        assert_eq!(failure.code, "TIMEOUT_ERROR");
        assert_eq!(
            failure.message,
            "Timeout error: db_write query timed out after 150ms"
        );
        assert!(failure.retryable);
    }
}
