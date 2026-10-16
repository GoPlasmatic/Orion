use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Function, Outcome};
use dataflow_rs_v3::engine::task_context::TaskContext;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, QueryBudget, QueryFailure, acquire_conn, encode_failure, reject_mongo_connector,
    require_op_allowed, to_connect_error,
};
use super::db_read::DbRead;
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::engine::HandlerError;
use crate::engine::kit;

/// This handler's name, for the shared write below (F48).
const NAME: &str = <DbWriteHandler as ConnectorHandler>::NAME;

/// Executes SQL write queries (INSERT, UPDATE, DELETE) against external databases
/// configured via connectors.
pub struct DbWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

/// The connector's gates for a raw write: its `raw_write` operation, and a
/// SQL backend. Shared by the dataflow-rs 3 handler and the dataflow-rs 4
/// function.
fn gate(conn: &DbConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    // Raw SQL cannot be classified per-op; it has its own gate.
    require_op_allowed(&conn.operations, "raw_write", connector)?;
    Ok(reject_mongo_connector(NAME, connector, conn)?)
}

#[async_trait]
impl ConnectorHandler for DbWriteHandler {
    const NAME: &'static str = "db_write";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    /// The same shape `db_read` parses — a literal statement and message-derived
    /// binds — because the two differ in what the database does with it, not in
    /// what the task says.
    type Parsed = DbRead<'static>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        DbRead::parse_statement(call, input, ctx)
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        gate(conn, connector)
    }

    async fn run(
        &self,
        write: Self::Parsed,
        db_config: &DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        Ok(
            write_rows(&self.pool_cache, call.connector, db_config, &write, None)
                .await?
                .into(),
        )
    }
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
            crate::sql_lex::leading_keyword(query).as_deref(),
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
    /// The SQL connector written through.
    connector: String,
    /// The write statement. Only `params` come from the message.
    query: String,
    /// The values bound to the statement's placeholders, in order.
    params: Option<Interpolated>,
    /// Where `{"rows_affected": n}` (and `"last_insert_id"`) is written.
    #[output(default = "data")]
    output: Output,
}

/// `db_write` as a dataflow-rs 4 function: the statement `DbWriteHandler`
/// runs, read in the same order.
pub struct DbWriteFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

impl Function for DbWriteFunction {
    type Config = DbWriteConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DbWriteConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let write = DbRead::statement(ctx, &cfg.query, cfg.params.as_ref(), NAME)?;
        let run_deadline = ctx.deadline();
        let mut answer = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
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

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DB_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to execute against.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "INSERT/UPDATE/DELETE statement. Bind placeholders are the backend's own spelling: ? for SQLite and MySQL, $1, $2, ... for PostgreSQL.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders, in order. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Array,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the write result is written: {\"rows_affected\": n}, plus \"last_insert_id\" for an INSERT on MySQL or SQLite (PostgreSQL reports generated columns through RETURNING instead).",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use dataflow_rs::run::ErrorKind;
    use serde_json::{Value, json};

    const SEED: &[&str] = &[
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
        "INSERT INTO items (name) VALUES ('apple')",
    ];

    fn handler(db: &twin::Sqlite) -> dataflow_rs_v3::BoxedFunctionHandler {
        Box::new(super::super::connector_handler::Connector(DbWriteHandler {
            pool_cache: db.pool_cache.clone(),
            registry: db.registry.clone(),
        }))
    }

    fn function(db: &twin::Sqlite) -> DbWriteFunction {
        DbWriteFunction {
            pool_cache: db.pool_cache.clone(),
            registry: db.registry.clone(),
        }
    }

    /// One task through each engine, each against a database of its own
    /// seeded with [`SEED`], its connector configured with `config`; and
    /// each database's rows after it.
    async fn both(input: Value, data: Value, config: Value) -> ((Seen, Value), (Seen, Value)) {
        let mut sides = Vec::new();
        for v4 in [false, true] {
            let db = twin::sqlite(config.clone(), SEED).await;
            let seen = if v4 {
                twin::v4(NAME, function(&db), input.clone(), data.clone()).await
            } else {
                twin::v3(NAME, handler(&db), input.clone(), data.clone()).await
            };
            let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM items ORDER BY id")
                .fetch_all(&db.pool)
                .await
                .expect("the table reads");
            sides.push((seen, json!(rows)));
        }
        let v4 = sides.pop().expect("v4");
        (sides.pop().expect("v3"), v4)
    }

    /// Declaration order is the field table's (I179); `params` is a document
    /// of `var` holes; `connector` and `query` are literals, required (I183);
    /// a key the table does not declare is refused (break 2a).
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = DbWriteConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = DB_WRITE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let params = json!([{"var": "data.name"}, {"$k": [1, {"var": "data.n"}]}]);
        let cfg = testing::config_with::<DbWriteConfig>(
            &crate::engine::kit::test_exprs(),
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
            testing::config_with::<DbWriteConfig>(&crate::engine::kit::test_exprs(), input)
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
    async fn a_write_answers_as_on_v3() {
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
            let (v3, v4) = both(input.clone(), json!({"name": "pear"}), json!({})).await;
            assert_eq!(v4, v3, "{input}");
            assert_eq!(v4.0.expect("written")["write"], answer, "{input}");
        }
        // With no `output`, the answer replaces `data`, an object a root
        // takes.
        let input = json!({"connector": "db", "query": "DELETE FROM items"});
        let (v3, v4) = both(input, json!({"keep": 1}), json!({})).await;
        assert_eq!(v4, v3);
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
            let (v3, v4) = both(input.clone(), data, config).await;
            assert_eq!(v4, v3, "{input}");
            assert!(v4.0.is_err(), "{input}");
            assert_eq!(v4.1, json!([[1, "apple"]]), "{input}");
        }
    }

    #[tokio::test]
    async fn a_mongo_connector_is_refused_as_on_v3() {
        let connector = json!({"type": "db", "connection_string": "mongodb://127.0.0.1:1"});
        let input = json!({"connector": "db", "query": "DELETE FROM t", "output": "data.write"});
        let pool_cache = Arc::new(SqlPoolCache::new(8));
        let registry = twin::connectors(&[("db", connector)]).await;
        let v3 = twin::v3(
            NAME,
            Box::new(super::super::connector_handler::Connector(DbWriteHandler {
                pool_cache: pool_cache.clone(),
                registry: registry.clone(),
            })),
            input.clone(),
            Value::Null,
        )
        .await;
        let v4 = twin::v4(
            NAME,
            DbWriteFunction {
                pool_cache,
                registry,
            },
            input,
            Value::Null,
        )
        .await;
        assert_eq!(v4, v3);
        assert_eq!(v4.expect_err("refused").code, "VALIDATION_ERROR");
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
            .exprs(crate::engine::kit::test_exprs())
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
        let config = json!({"query_timeout_ms": 150});
        let db = twin::sqlite(config.clone(), SEED).await;
        let held = db.pool.acquire().await.expect("the one connection");
        let v3 = twin::v3(NAME, handler(&db), delete_all(), Value::Null).await;
        drop(held);
        let db = twin::sqlite(config, SEED).await;
        let _held = db.pool.acquire().await.expect("the one connection");
        let called = testing::call(function(&db), delete_all())
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
            .await;
        let v4 = twin::seen(&called);
        assert_eq!(v4, v3);
        let failure = v4.expect_err("the write timed out");
        assert_eq!(failure.code, "TIMEOUT_ERROR");
        assert_eq!(
            failure.message,
            "Timeout error: db_write query timed out after 150ms"
        );
        assert!(failure.retryable);
    }
}
