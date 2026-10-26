use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use serde_json::Value;

use super::connector_helpers::{
    QueryBudget, QueryFailure, acquire_conn, decode_failure, encode_failure, require_op_allowed,
    to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::sql_decode::RowFormat;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::kit::{self, read};
use crate::{ErrorClass, HandlerError};

/// This handler's name, for the row-conversion helpers below — a reference to
/// the one place it is written (F48), not a second spelling of it.
pub(crate) const NAME: &str = "db_read";

/// The statement and its bind values.
///
/// The SQL text is a literal read from the task; only the parameters come from
/// the message, which is what keeps them the sole request-controlled part of
/// the statement. Owned by the dataflow-rs 3 handlers, which read it off the
/// authored input per call; borrowed from the compiled config by the
/// dataflow-rs 4 functions.
pub struct DbRead<'q> {
    query: Cow<'q, str>,
    params: Vec<Value>,
    format: RowFormat,
}

impl<'q> DbRead<'q> {
    /// The statement a dataflow-rs 4 config states, its binds rendered for
    /// this call: [`DbRead::parse_statement`]'s reading, the query borrowed.
    pub(super) fn statement(
        ctx: &Ctx<'_>,
        query: &'q str,
        params: Option<&Interpolated>,
        handler: &str,
    ) -> Result<Self, HandlerError> {
        Ok(Self {
            query: Cow::Borrowed(query),
            params: rendered_binds(ctx, params, handler)?,
            format: RowFormat::default(),
        })
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn params(&self) -> &[Value] {
        &self.params
    }

    pub(super) fn format(&self) -> RowFormat {
        self.format
    }
}

/// A `params` document rendered for this call, as `resolve_bind_params` reads
/// it: unset or `null` binds nothing, an array binds its elements, anything
/// else is refused. The rendered document serializes straight into the JSON
/// the reader takes, with no tree of its own.
pub(super) fn rendered_binds(
    ctx: &Ctx<'_>,
    params: Option<&Interpolated>,
    handler: &str,
) -> Result<Vec<Value>, HandlerError> {
    let rendered = params.map(|doc| kit::rendered(ctx, doc)).transpose()?;
    read::bind_params(rendered, handler)
}

/// `numeric_as` and `binary_as` rendered for this call, `numeric_as` read
/// first, as `resolve_row_format` reads them.
pub(super) fn rendered_row_format(
    ctx: &Ctx<'_>,
    numeric_as: Option<&Interpolated>,
    binary_as: Option<&Interpolated>,
    handler: &str,
) -> Result<RowFormat, HandlerError> {
    let numeric = numeric_as.map(|doc| ctx.render(doc).to_owned());
    let binary = binary_as.map(|doc| ctx.render(doc).to_owned());
    read::row_format(numeric.as_ref(), binary.as_ref(), handler)
}

/// The connector's gate for a read: its `read` operation. Shared by the
/// dataflow-rs 3 handler and the dataflow-rs 4 function.
fn gate(conn: &DbConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    require_op_allowed(&conn.operations, "read", connector)
}

/// The rows a read returns, as a JSON array, against a resolved and gated
/// connector. Shared by the dataflow-rs 3 handler and the dataflow-rs 4
/// function, so both run the same statement the same way. `run_deadline`
/// is the deadline of the run the call belongs to, which the dataflow-rs 4
/// function passes on ([`QueryBudget::within`]).
async fn read_rows(
    pool_cache: &SqlPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    max_rows: usize,
    max_result_bytes: usize,
    read: &DbRead<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<Value, HandlerError> {
    let pool = pool_cache
        .get_pool(connector, db_config)
        .await
        .map_err(to_connect_error)?;

    let params = read.params();
    let format = read.format();
    let query = read.query();

    // One budget over both legs. `acquire` is a round trip of its own, and
    // giving it a timeout of its own would silently make the bound the
    // connector's owner set into `connect_timeout_ms` plus
    // `query_timeout_ms`.
    let budget = QueryBudget::within(db_config.query_timeout_ms, run_deadline);

    // Bound once, outside the dispatch, because the vocabulary is the same
    // whichever driver runs it.
    let scalars: Vec<crate::connector::sql_encode::Scalar> =
        params.iter().map(Into::into).collect();

    // One body, three drivers: the macro binds the concrete pool, its
    // decoder and its binders, and each arm is type-checked on its own.
    let json = crate::connector::pool_cache::dispatch_sql_pool!(
        &pool, p, rows_to_json, bind, typed_args, _write_result => {
            // The connection is named rather than left to sqlx to acquire
            // per statement, because PostgreSQL caches a prepared
            // statement's parameter types per connection — asking the
            // server what it declared and then binding against it only
            // means anything if both happen on the same one.
            let mut conn = acquire_conn(&budget, NAME, p).await?;
            // Another leg of the same budget: on a connection that has
            // already seen this SQL it is a cache hit and no round trip at
            // all, but the first time through it is a real one.
            let bound = budget
                .run(NAME, async {
                    typed_args(&mut conn, query, Some(&scalars))
                        .await
                        .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                })
                .await?;
            budget.run(NAME, async {
                use futures::TryStreamExt;
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
                let mut stream = sqlx_query.fetch(&mut *conn);
                let mut json_rows = Vec::new();
                let mut total_bytes = 0usize;
                // Not `.map_err(|e| e.to_string())`: stringifying here
                // converted through `From<String>`, which is
                // unconditionally a backend failure, so a constraint the
                // driver had already classified was thrown away before
                // `QueryFailure` could see it.
                while let Some(row) = stream.try_next().await? {
                    if json_rows.len() >= max_rows {
                        // F42: classified so `timed_query` reports it as a
                        // 400 with the text intact rather than a 500 with
                        // the guidance sanitised away. The guidance *is*
                        // the message, so losing it loses the point.
                        return Err(
                            crate::functions::connector_helpers::QueryFailure::Limit(
                                format!(
                                    "{NAME} result exceeds query.max_limit ({max_rows} rows) \
                                     — add a LIMIT to the query or raise the cap"
                                ),
                            ),
                        );
                    }
                    let mut decoded = rows_to_json(std::slice::from_ref(&row), format)
                        .map_err(|e| QueryFailure::Classified(decode_failure(NAME, e)))?;
                    if let Some(decoded_row) = decoded.pop() {
                        if max_result_bytes > 0 {
                            total_bytes = total_bytes.saturating_add(
                                crate::connector::sql_decode::approx_json_value_bytes(&decoded_row),
                            );
                            if total_bytes > max_result_bytes {
                                return Err(
                                    crate::functions::connector_helpers::QueryFailure::Limit(
                                        format!(
                                            "{NAME} result exceeds query.max_result_bytes ({max_result_bytes} bytes) \
                                             — select fewer columns, add a LIMIT, or raise the cap"
                                        ),
                                    ),
                                );
                            }
                        }
                        json_rows.push(decoded_row);
                    }
                }
                Ok(json_rows)
            })
            .await?
        }
    );

    Ok(Value::Array(json))
}

/// I185's Defect: the rows are an array, which no context root takes (break
/// 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`db_read` writes an array of rows, and a context root takes \
    only an object (break 21): set `output` to a path below it, such as `data.rows`";

/// `db_read`'s input on dataflow-rs 4, in its field table's order (I179). The
/// connector and the statement are literals, required at load (I183); the
/// binds and the two rendering choices are documents of `var` holes, as 1.12
/// folded them, read by `kit::read`'s twins of 1.12's readers.
#[derive(Debug, FunctionConfig)]
pub struct DbReadConfig {
    /// The SQL connector queried.
    connector: String,
    /// The read statement. Only `params` come from the message.
    query: String,
    /// The values bound to the statement's placeholders, in order.
    params: Option<Interpolated>,
    /// How a decimal column renders: `number` (the default) or `string`.
    numeric_as: Option<Interpolated>,
    /// How a binary column renders: `auto` (the default), `hex`, `base64`
    /// or `text`.
    binary_as: Option<Interpolated>,
    /// Where the rows are written.
    #[output(default = "data")]
    output: Output,
}

/// `db_read` as a dataflow-rs 4 function: the statement `DbReadHandler`
/// runs, read in the same order.
pub struct DbReadFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Hard row cap, from `query.max_limit` (F10).
    pub max_rows: usize,
    /// Hard decoded byte cap across the result set, from
    /// `query.max_result_bytes` (`0` = unlimited, `TD-09`).
    pub max_result_bytes: usize,
}

impl Function for DbReadFunction {
    type Config = DbReadConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    /// I185: an `output` that is a literal context root, the default `data`
    /// included, is a Defect at `output`.
    fn prepare(&self, cfg: &mut DbReadConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DbReadConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse_read`'s order: the rendering
        // choices, the binds, the read-only check.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let format =
            rendered_row_format(ctx, cfg.numeric_as.as_ref(), cfg.binary_as.as_ref(), NAME)?;
        let read = DbRead {
            format,
            ..DbRead::statement(ctx, &cfg.query, cfg.params.as_ref(), NAME)?
        };
        require_read_only(&read.query, NAME)?;
        let run_deadline = ctx.deadline();
        let mut rows = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let db = kit::require_connector::<crate::connector::kind::Db>(&config, &cfg.connector)?;
            gate(db, &cfg.connector)?;
            rows = Some(
                read_rows(
                    &self.pool_cache,
                    &cfg.connector,
                    db,
                    self.max_rows,
                    self.max_result_bytes,
                    &read,
                    run_deadline,
                )
                .await?,
            );
            Ok(Outcome::Ok)
        })
        .await?;
        if let Some(rows) = rows {
            ctx.set(&output, dataflow_rs::datavalue::OwnedDataValue::from(&rows))?;
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

pub(super) const DB_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to query.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "Read statement — SELECT, WITH, VALUES or TABLE; a write belongs in db_write, which has its own 'raw_write' connector gate. Bind placeholders are the backend's own spelling: ? for SQLite and MySQL, $1, $2, ... for PostgreSQL.",
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
        name: "numeric_as",
        description: "How an arbitrary-precision decimal column is rendered: \"number\" (default) or \"string\". A number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a string keeps every digit, which is what a money column needs.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "binary_as",
        description: "How a binary column is rendered: \"auto\" (default), \"hex\", \"base64\" or \"text\". Auto reads the bytes as text when they are valid UTF-8 and as hex when they are not, so its result shape depends on the data; name an encoding for a column that is genuinely binary.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

// -- Read-only statement check --
//
// `db_read` gates on the connector's `read` operation and then runs whatever
// statement the task carries. `fetch` executes any statement — it merely
// streams whatever rows come back — so `DELETE FROM t RETURNING id` ran here on
// PostgreSQL and SQLite, and a bare `DELETE`/`UPDATE`/`INSERT` ran (returning no
// rows) on all three, while `raw_write: false` was set on the connector.
//
// That made the operation gates advertise more than they enforced. The gate
// table's own claim — "SQL writes are fully bounded by allowed_entities once
// `raw_write: false` leaves `data_write` as the only write path" — is only true
// with this check in place, because `db_read` was the second write path.
//
// The statement is a workflow-authored literal, never caller-supplied, so this
// is not an injection guard; it is what makes "delete-proof connector" a
// property an operator can rely on rather than a convention authors are asked
// to keep.

/// Refuse a `db_read` statement that is not a read, in the words the
/// handler has always used. The judgement is [`crate::sql_lex`]'s — the one
/// lexer every surface reads SQL with — read lossily, as the run-time check
/// always has been.
///
/// # Errors
///
/// [`DataflowError::Validation`] when the statement does not open with one of
/// [`crate::sql_lex::READ_STATEMENTS`], or when it carries a data-modifying
/// CTE.
fn require_read_only(query: &str, handler_name: &str) -> Result<(), HandlerError> {
    use crate::sql_lex::{READ_STATEMENTS, ReadOnlyViolation};
    let message = match crate::sql_lex::read_only_violation(query) {
        None => return Ok(()),
        Some(ReadOnlyViolation::Empty) => format!("{handler_name} 'query' has no statement to run"),
        Some(ReadOnlyViolation::NotARead { keyword }) => format!(
            "{handler_name} runs read statements only, but this one starts with \
             '{keyword}' — use db_write for INSERT/UPDATE/DELETE (it has its own \
             'raw_write' connector gate). Reads start with {}",
            READ_STATEMENTS.join(", ")
        ),
        Some(ReadOnlyViolation::ModifyingCte { keyword }) => format!(
            "{handler_name} runs read statements only, but this one carries a \
             data-modifying '{keyword}' common table expression — use db_write \
             (it has its own 'raw_write' connector gate)"
        ),
    };
    Err(HandlerError::new(ErrorClass::CallerInput, message))
}

#[cfg(test)]
mod tests {
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
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = DbReadConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = DB_READ_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

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
        let select =
            json!({"connector": "db", "query": "SELECT id FROM items", "output": "data.rows"});
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
}
