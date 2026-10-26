use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Function, Outcome};
use serde_json::{Map, Value, json};

use super::connector_helpers::{
    QueryBudget, QueryFailure, acquire_conn, build_entity_registry, decode_failure, encode_failure,
    require_op_allowed, to_connect_error, to_exec_error,
};
use super::db_read::rendered_row_format;
use super::schema::{FieldKind, FieldSchema};
use crate::HandlerError;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::sql_decode::RowFormat;
use crate::connector::{ConnectorConfig, ConnectorRegistry};
use crate::kit::{self, read};
use crate::query::write::{ResolvedWrite, WriteOp};
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
pub(crate) const NAME: &str = "data_write";

/// The mutation as the task states it, with `params` folded against the
/// message.
///
/// The envelope is carried *unresolved* on purpose: resolving it needs the
/// entity registry, which is built from the connector's own schema guards, so
/// there is nothing to resolve until the connector is in hand.
///
/// The literals are borrowed from the function's compiled config.
pub struct DataWrite<'a> {
    params: Map<String, Value>,
    /// How a decimal and a binary column in a `returning` row render. SQL
    /// backends only.
    format: RowFormat,
    schema: Option<Cow<'a, Value>>,
    envelope: Cow<'a, Value>,
}

/// What a portable write reaches its backend through, and the bounds it is
/// held to: the handler's or the function's, borrowed.
struct Backends<'a> {
    pool_cache: &'a SqlPoolCache,
    write_config: &'a crate::config::WriteConfig,
    max_returning: usize,
}

/// The write's answer and its outcome against a resolved `db` connector: the
/// envelope resolved, its operation gated, then written. `run_deadline` is the
/// deadline of the run the call belongs to, which bounds every SQL leg with
/// the connector's `query_timeout_ms` ([`QueryBudget::within`]).
async fn run_write(
    backends: Backends<'_>,
    parsed: &DataWrite<'_>,
    conn: &ConnectorConfig,
    connector: &str,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<(Value, Outcome), HandlerError> {
    // F24: with no schema the registry rejects rather than passing every
    // name through, and the connector's own guards apply on top.
    let registry = build_entity_registry(parsed.schema.as_deref(), conn, connector)?;

    // One backend-neutral resolution: parse envelope, fold params into
    // values/set, resolve physical names, coerce, lower the filter, and
    // enforce the write-safety guards (W15).
    let resolved = query::write::resolve_write(
        &parsed.envelope,
        &parsed.params,
        &registry,
        backends.write_config,
    )
    .map_err(HandlerError::from)?;

    // The gate, before anything is dialled: a connector config can disable
    // individual ops (e.g. delete) regardless of what workflows ask for.
    if let Some(gates) = conn.operation_gates() {
        require_op_allowed(gates, resolved.op().as_str(), connector)?;
    }

    Ok(match conn {
        ConnectorConfig::Db(db) => {
            let dialect: SqlDialect = detect_backend(&db.connection_string)
                .map_err(to_exec_error)?
                .into();
            let (sql, values) = query::backend::sql::render_write(&resolved, dialect)
                .map_err(HandlerError::from)?;
            let pool = backends
                .pool_cache
                .get_pool(connector, db)
                .await
                .map_err(to_connect_error)?;
            execute_sql(
                &pool,
                &sql,
                values,
                &resolved,
                QueryBudget::within(db.query_timeout_ms, run_deadline),
                parsed.format,
                backends.max_returning,
            )
            .await?
        }
        // `DataBackend` admits `db` and nothing else; the wrong-type
        // refusal this arm used to write is the wrapper's now.
        other => unreachable!(
            "DataBackend admitted a '{}' connector",
            other.connector_type()
        ),
    })
}

/// `data_write`'s input on dataflow-rs 4, in its field table's order (I179).
/// The connector and the envelope are required at load (I183). The envelope
/// and the schema are literal documents, `{"param": …}` holes and all, for
/// Orion's own dialect to fold; `params` and the rendering choices are
/// documents of `var` holes, as 1.12 folded them.
#[derive(Debug, FunctionConfig)]
pub struct DataWriteConfig {
    /// The `db` connector written through.
    connector: String,
    /// The backend-neutral mutation envelope.
    write: Value,
    /// The named values folded into the envelope's `{"param": …}` nodes.
    params: Option<Interpolated>,
    /// The inline entity schema.
    schema: Option<Value>,
    /// How a decimal column in a `returning` row renders. SQL backends only.
    numeric_as: Option<Interpolated>,
    /// How a binary column in a `returning` row renders. SQL backends only.
    binary_as: Option<Interpolated>,
    /// Where the write's answer is written.
    #[output(default = "data")]
    output: Output,
}

/// `data_write` as a dataflow-rs 4 function: the write `DataWriteHandler`
/// makes, read in the same order.
pub struct DataWriteFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Safety bounds (`max_rows` / `allow_unfiltered`) from `[write]`.
    pub write_config: crate::config::WriteConfig,
    /// Hard cap on rows a `returning` clause may hand back, from
    /// `query.max_limit`.
    pub max_returning: usize,
}

impl Function for DataWriteFunction {
    type Config = DataWriteConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DataWriteConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse`'s order: `params`, the
        // rendering choices, the literals.
        let output = kit::output_path(ctx, &cfg.output, Some(&kit::DATA))?;
        let params = cfg
            .params
            .as_ref()
            .map(|doc| kit::rendered(ctx, doc))
            .transpose()?;
        let parsed = DataWrite {
            params: read::params(params),
            format: rendered_row_format(
                ctx,
                cfg.numeric_as.as_ref(),
                cfg.binary_as.as_ref(),
                NAME,
            )?,
            schema: cfg.schema.as_ref().map(Cow::Borrowed),
            envelope: Cow::Borrowed(&cfg.write),
        };
        let run_deadline = ctx.deadline();
        let mut answer = None;
        let done = kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::DataBackend>(&config, &cfg.connector)?;
            let backends = Backends {
                pool_cache: &self.pool_cache,
                write_config: &self.write_config,
                max_returning: self.max_returning,
            };
            let (result, done) =
                run_write(backends, &parsed, conn, &cfg.connector, run_deadline).await?;
            answer = Some(result);
            Ok(done)
        })
        .await?;
        if let Some(answer) = answer {
            ctx.set(
                &output,
                dataflow_rs::datavalue::OwnedDataValue::from(&answer),
            )?;
        }
        Ok(done)
    }
}

/// Execute a rendered SQL write.
///
/// F28: a **multi-row** write runs inside an explicit transaction. A bulk
/// insert renders to a single `INSERT … VALUES (…), (…)`, which every supported
/// engine already applies atomically under autocommit — so this changes no
/// observable behaviour today. It makes SQL's all-or-nothing guarantee, which
/// the dialect's documented contract now promises, a property of this function
/// rather than of the renderer continuing to emit exactly one statement.
///
/// A single-row write is left on autocommit deliberately: one statement is
/// already atomic, and `BEGIN`/`COMMIT` around it would put two extra round
/// trips on the hot path — and, on SQLite, hold the database's single write
/// lock across them instead of releasing it when the statement ends.
///
/// The one exception is an `update`/`delete` that asks for `returning`
/// ([`ResolvedWrite::returns_unbounded_rows`]). Its result set is bounded by
/// `max_returning`, and a write that trips that cap must leave nothing behind:
/// reporting a 400 over a statement that had already committed would be worse
/// than the unbounded read it replaces. The transaction is what makes the
/// refusal honest. An `insert … returning` keeps the fast path — its result set
/// is one row per row written, already capped by `write.max_rows`.
///
/// Every round trip shares one [`QueryBudget`]: `Pool::begin` acquires a
/// connection *and* sends `BEGIN`, and the commit is another round trip, so
/// leaving either unbounded would reintroduce the hang F11 closed, and giving
/// each its own `query_timeout_ms` would triple the bound the connector's owner
/// configured.
async fn execute_sql(
    pool: &crate::connector::pool_cache::SqlPool,
    sql: &str,
    values: sea_query_sqlx::SqlxValues,
    w: &ResolvedWrite,
    budget: QueryBudget,
    format: crate::connector::sql_decode::RowFormat,
    max_returning: usize,
) -> Result<(Value, Outcome), HandlerError> {
    // The transaction is per driver, so the whole statement runs inside the
    // dispatch rather than behind an `Executor` bound — `sqlx::Any` was the
    // only thing that made one generic body possible, and it is what capped
    // the type vocabulary (#309).
    let mut out = crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, _bind, typed_args, write_result => {
            let scalars = crate::connector::sql_encode::scalars_from_sea(&values.0);
            // Converted up front, with the pool pinning which driver's
            // arguments these are; the move itself is free.
            let fallback = crate::connector::sql_encode::sea_args_for(p, values);
            if w.is_multi_row() || w.returns_unbounded_rows() {
                let mut tx = budget.run(NAME, p.begin()).await?;
                // On the transaction's own connection: that is where the
                // statement will run, and a prepared statement's parameter
                // types are cached per connection.
                let bound = budget
                    .run(NAME, async {
                        typed_args(&mut tx, sql, scalars.as_deref())
                            .await
                            .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                    })
                    .await?;
                let (args, persistent) = decide(bound, fallback);
                let out = run_write_statement(
                    &mut *tx, sql, args, persistent, w, &budget, rows_to_json, write_result,
                    format, max_returning,
                )
                .await?;
                budget.run(NAME, tx.commit()).await?;
                out
            } else {
                // The transaction branch above already owns its connection;
                // this one names its own for the same reason — a prepared
                // statement's parameter types are cached per connection, so
                // the prepare and the execute have to land on the same one.
                let mut conn = acquire_conn(&budget, NAME, p).await?;
                let bound = budget
                    .run(NAME, async {
                        typed_args(&mut conn, sql, scalars.as_deref())
                            .await
                            .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                    })
                    .await?;
                let (args, persistent) = decide(bound, fallback);
                run_write_statement(
                    &mut *conn, sql, args, persistent, w, &budget, rows_to_json, write_result,
                    format, max_returning,
                )
                .await?
            }
        }
    );
    // SQL writes are atomic, so the status is always `ok`; the key is part
    // of the result's documented shape.
    out["status"] = json!("ok");
    Ok((out, Outcome::Ok))
}

/// Settle a [`Bound`] into the arguments to send and whether the statement may
/// be cached.
///
/// `Typed` is always cacheable: its types are the ones the server inferred, so
/// re-preparing under them declares the same statement again.
fn decide<A>(bound: crate::connector::sql_encode::Bound<A>, fallback: A) -> (A, bool) {
    match bound {
        crate::connector::sql_encode::Bound::Typed(args) => (args, true),
        crate::connector::sql_encode::Bound::Fallback { cache } => (fallback, cache),
    }
}

/// Run the rendered statement on `executor` (the pool, or a transaction's
/// connection). When `returning` is requested the statement's rows are streamed
/// and counted against `max_returning`; otherwise it is a plain `execute`
/// returning the affected-row count (and `last_insert_id` where the driver
/// reports one).
///
/// Streamed rather than `fetch_all`ed because the cap has to bite before the
/// rows are in memory — materialising a million-row `returning` set and then
/// measuring it is the defect, not the check. The refusal is a
/// [`QueryFailure::Limit`], so it reaches the caller as a 400 with the guidance
/// intact, exactly like `db_read`'s row cap.
///
/// [`QueryFailure::Limit`]: super::connector_helpers::QueryFailure::Limit
#[allow(clippy::too_many_arguments)]
async fn run_write_statement<'e, E, R, F, G>(
    executor: E,
    sql: &'e str,
    args: R::Arguments,
    persistent: bool,
    w: &ResolvedWrite,
    budget: &QueryBudget,
    rows_to_json: F,
    write_result: G,
    format: crate::connector::sql_decode::RowFormat,
    max_returning: usize,
) -> Result<Value, HandlerError>
where
    E: sqlx::Executor<'e, Database = R>,
    R: sqlx::Database + sqlx::database::HasStatementCache,
    // No blanket impl relates a database to its own arguments type, so the
    // bound has to be spelled out.
    R::Arguments: sqlx::IntoArguments<R>,
    F: Fn(
        &[R::Row],
        crate::connector::sql_decode::RowFormat,
    ) -> Result<Vec<Value>, crate::connector::sql_decode::DecodeError>,
    G: Fn(&R::QueryResult) -> (u64, Option<i64>),
{
    if w.returning().is_empty() {
        let res = budget
            .run(
                NAME,
                sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                    .persistent(persistent)
                    .execute(executor),
            )
            .await?;
        let (rows_affected, last_insert_id) = write_result(&res);
        let mut out = json!({ "rows_affected": rows_affected });
        // MySQL and SQLite report an auto-increment id; PostgreSQL does not,
        // and never did — `AnyQueryResult` reported `0` there, so the key was
        // present and meaningless. Postgres uses `RETURNING`, which this
        // dialect already supports.
        if matches!(w.op(), WriteOp::Insert | WriteOp::Upsert)
            && let Some(id) = last_insert_id
        {
            out["last_insert_id"] = json!(id);
        }
        Ok(out)
    } else {
        let rows = budget
            .run(NAME, async {
                use futures::TryStreamExt;
                let mut stream = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                    .persistent(persistent)
                    .fetch(executor);
                let mut rows = Vec::new();
                while let Some(row) = stream.try_next().await? {
                    if rows.len() >= max_returning {
                        return Err(super::connector_helpers::QueryFailure::Limit(format!(
                            "{NAME} 'returning' set exceeds query.max_limit                              ({max_returning} rows) — narrow the filter, drop                              'returning', or raise the cap"
                        )));
                    }
                    rows.push(row);
                }
                Ok(rows)
            })
            .await?;
        let returning = rows_to_json(&rows, format).map_err(|e| decode_failure(NAME, e))?;
        let count = returning.len();
        Ok(json!({ "rows_affected": count, "returning": returning }))
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DATA_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the db (SQL) connector to write to.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "write",
        description: "Backend-neutral mutation envelope: op/target/values/set/filter/on_conflict/returning/all.",
        kind: FieldKind::Object,
        // W7: required. The pre-1.0 flat form (envelope keys at the top level)
        // is gone, so there is exactly one shape and the generic
        // missing-required-field rule reports it — at create, update, import,
        // `POST /admin/workflows/validate` and `orion-cli lint`, rather than
        // at the task's first request.
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into {\"param\": ..} nodes in values/set/filter. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "schema",
        description: "Inline entity schema (renames, allowlist, writable flag). Undeclared \
                      entities and columns are rejected, so a write without one reaches \
                      nothing; pass {\"unmapped\": \"identity\"} for pre-1.0 pass-through.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "numeric_as",
        description: "How an arbitrary-precision decimal column is rendered: \"number\" (default) or \"string\". A number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a string keeps every digit, which is what a money column needs. SQL backends only.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "binary_as",
        description: "How a binary column is rendered: \"auto\" (default), \"hex\", \"base64\" or \"text\". Auto reads the bytes as text when they are valid UTF-8 and as hex when they are not, so its result shape depends on the data; name an encoding for a column that is genuinely binary. SQL backends only.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

pub(super) const DATA_WRITE_ENVELOPE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "op",
        description: "Mutation kind: \"insert\", \"update\", \"delete\", or \"upsert\".",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "target",
        description: "Logical entity to write to (schema-resolved to a table).",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "values",
        description: "Row object, or array of row objects (bulk), for insert/upsert.",
        kind: FieldKind::Any,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "set",
        description: "Object of column → value assignments for update/upsert.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "filter",
        description: "Query-dialect filter selecting rows for update/delete. An \
                      update/delete without it is rejected unless \"all\": true is set.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "on_conflict",
        description: "Upsert conflict clause: { \"target\": [cols], \"action\": \"update\"|\"nothing\" }.",
        kind: FieldKind::Object,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "returning",
        description: "Column names to return from mutated rows (Postgres/SQLite only).",
        kind: FieldKind::Array,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/delete (affects every row).",
        kind: FieldKind::Bool,
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::functions::twin::{self, Seen};
    use dataflow_rs::ErrorKind;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};

    const SEED: &[&str] = &[
        "CREATE TABLE users (id TEXT PRIMARY KEY, status TEXT, avatar BLOB)",
        "INSERT INTO users VALUES ('u1', 'active', X'00FF')",
    ];

    fn function(
        pool_cache: Arc<SqlPoolCache>,
        registry: Arc<ConnectorRegistry>,
    ) -> DataWriteFunction {
        DataWriteFunction {
            pool_cache,
            registry,
            write_config: crate::config::WriteConfig::default(),
            max_returning: 1000,
        }
    }

    /// The rows of `users`, in order.
    async fn users(db: &twin::Sqlite) -> Value {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, status FROM users ORDER BY id")
                .fetch_all(&db.pool)
                .await
                .expect("the table reads");
        json!(rows)
    }

    /// One task against a database of its own seeded with [`SEED`], its
    /// connector configured with `config`; and the database's rows after it.
    async fn run(input: Value, data: Value, config: Value) -> (Seen, Value) {
        let db = twin::sqlite(config, SEED).await;
        let (pool_cache, registry) = (db.pool_cache.clone(), db.registry.clone());
        let seen = twin::v4(NAME, function(pool_cache, registry), input, data).await;
        (seen, users(&db).await)
    }

    /// `seen` and the rows after it, each against 1.12's record under `key`.
    #[track_caller]
    fn pinned(seen: &(Seen, Value), key: &str) {
        twin::pinned(&seen.0, key);
        twin::pinned(&Ok(seen.1.clone()), format!("{key}/rows"));
    }

    /// A `data_write` task of `envelope` over `users`, every column
    /// writable.
    fn write(envelope: Value) -> Value {
        json!({
            "connector": "db", "write": envelope,
            "schema": {"unmapped": "identity"}, "output": "data.w"
        })
    }

    /// Declaration order is the field table's (I179). The envelope and the
    /// schema are literal documents, `{"param": …}` holes and `$` keys as
    /// authored; `params` is a document of `var` holes; `connector` and
    /// `write` are required (I183), where 1.12 failed each call without
    /// them; an unknown key is refused (break 2a), the flat pre-1.0 envelope
    /// included.
    #[test]
    fn the_config_is_the_field_table() {
        let fields: Vec<&str> = DataWriteConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = DATA_WRITE_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);

        let envelope = json!({
            "op": "update", "target": "users",
            "set": {"status": {"param": "s"}, "$meta": {"var": "data.x"}},
            "filter": {"==": [{"field": "id"}, {"param": "id"}]}
        });
        let params = json!({"s": {"var": "data.status"}, "id": {"var": "data.who"}});
        let cfg = testing::config_with::<DataWriteConfig>(
            &crate::kit::test_exprs(),
            json!({"connector": "db", "write": envelope.clone(), "params": params.clone()}),
        )
        .expect("compiles");
        assert_eq!(cfg.write, envelope, "the envelope is literal");
        let doc = cfg.params.as_ref().expect("set");
        assert_eq!(doc.raw(), &params);
        let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.status", "data.who"]);
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));

        let rejected = |input: Value| -> Vec<(String, IssueCode)> {
            testing::config_with::<DataWriteConfig>(&crate::kit::test_exprs(), input)
                .expect_err("refused")
                .into_iter()
                .filter(|i| i.severity == Severity::Rejected)
                .map(|i| (i.path.to_string(), i.code))
                .collect()
        };
        assert_eq!(
            rejected(json!({"connector": "db"})),
            [("write".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"write": {}})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        let flat = rejected(json!({"connector": "db", "write": {}, "op": "insert"}));
        assert_eq!(flat.len(), 1, "{flat:?}");
        assert_eq!(flat[0].0, "op");
    }

    /// An insert, a bulk insert (in its transaction), an update folding
    /// params from the message, and a delete with `returning`, each
    /// answered at `output`, each database ending as 1.12's did.
    #[tokio::test]
    async fn a_write_answers_as_1_12_did() {
        let data = json!({"status": "banned", "who": "u1"});
        let mut update = write(json!({
            "op": "update", "target": "users",
            "set": {"status": {"param": "s"}},
            "filter": {"==": [{"field": "id"}, {"param": "id"}]}
        }));
        update["params"] = json!({"s": {"var": "data.status"}, "id": {"var": "data.who"}});
        for input in [
            write(
                json!({"op": "insert", "target": "users", "values": {"id": "u2", "status": "new"}}),
            ),
            write(json!({"op": "insert", "target": "users", "values": [
                {"id": "u2", "status": "a"}, {"id": "u3", "status": "b"}
            ]})),
            update,
            write(json!({
                "op": "delete", "target": "users",
                "filter": {"==": [{"field": "id"}, "u1"]}, "returning": ["id", "avatar"]
            })),
        ] {
            let v4 = run(input.clone(), data.clone(), json!({})).await;
            pinned(&v4, &format!("a_write_answers/{input}"));
            assert_eq!(v4.0.expect("written")["w"]["status"], "ok", "{input}");
        }
        // `binary_as` folds into a `returning` row's rendering.
        let mut input = write(json!({
            "op": "delete", "target": "users",
            "filter": {"==": [{"field": "id"}, "u1"]}, "returning": ["avatar"]
        }));
        input["binary_as"] = json!({"var": "data.enc"});
        let v4 = run(input, json!({"enc": "base64"}), json!({})).await;
        pinned(&v4, "a_write_answers/binary_as");
        assert_eq!(
            v4.0.expect("deleted")["w"]["returning"],
            json!([{"avatar": "AP8="}])
        );
    }

    /// The operation's gate, the write-safety guards, a missing param, the
    /// integrity refusal, the connector and an output that names no path,
    /// each with 1.12's code and text, and each database untouched.
    #[tokio::test]
    async fn refusals_are_1_12s() {
        let insert = write(json!({"op": "insert", "target": "users", "values": {"id": "u2"}}));
        let mut absent = insert.clone();
        absent["connector"] = json!("absent");
        let mut computed = insert.clone();
        computed["output"] = json!({"var": "data.n"});
        let mut binary = insert.clone();
        binary["binary_as"] = json!("money");
        for (input, config) in [
            (insert.clone(), json!({"operations": {"insert": false}})),
            (write(json!({"op": "delete", "target": "users"})), json!({})),
            (
                write(
                    json!({"op": "update", "target": "users", "set": {"status": {"param": "s"}}, "all": true}),
                ),
                json!({}),
            ),
            (
                write(json!({"op": "insert", "target": "users", "values": {"id": "u1"}})),
                json!({}),
            ),
            (absent, json!({})),
            (computed, json!({})),
            (binary, json!({})),
        ] {
            let key = format!("refusals/{input} {config}");
            let v4 = run(input.clone(), json!({"n": 1}), config).await;
            pinned(&v4, &key);
            assert!(v4.0.is_err(), "{input}");
            assert_eq!(v4.1, json!([["u1", "active"]]), "{input}");
        }
    }

    fn insert_one() -> Value {
        write(json!({"op": "insert", "target": "users", "values": {"id": "u2"}}))
    }

    /// `QueryBudget` within the run's deadline (E10), the one connection held
    /// so the write waits on the pool: a run deadline before the connector's
    /// `query_timeout_ms` ends the call `TimedOut`, as 1.12's channel timeout
    /// ended the run, writing nothing, here or in the database.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        let db = twin::sqlite(json!({"query_timeout_ms": 30_000}), SEED).await;
        let held = db.pool.acquire().await.expect("the one connection");
        let started = std::time::Instant::now();
        let called = testing::call(
            function(db.pool_cache.clone(), db.registry.clone()),
            insert_one(),
        )
        .exprs(crate::kit::test_exprs())
        .name(NAME)
        .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
        .await;
        let error = called.outcome.expect_err("the run's deadline passed");
        assert_eq!(*error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.message(),
            "the run's deadline passed during `data_write`"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(called.writes.is_empty());
        drop(held);
        assert_eq!(users(&db).await, json!([["u1", "active"]]));
    }

    /// A `query_timeout_ms` before the run's deadline is the budget's, with
    /// 1.12's code, text and retryability, on autocommit and inside a bulk
    /// insert's transaction.
    #[tokio::test]
    async fn a_query_timeout_before_the_run_deadline_is_1_12s() {
        let config = json!({"query_timeout_ms": 150});
        let bulk = write(
            json!({"op": "insert", "target": "users", "values": [{"id": "u2"}, {"id": "u3"}]}),
        );
        for input in [insert_one(), bulk] {
            let db = twin::sqlite(config.clone(), SEED).await;
            let _held = db.pool.acquire().await.expect("the one connection");
            let called = testing::call(
                function(db.pool_cache.clone(), db.registry.clone()),
                input.clone(),
            )
            .exprs(crate::kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
            .await;
            let v4 = twin::seen(&called);
            twin::pinned(
                &v4,
                format!("a_query_timeout_before_the_run_deadline/{input}"),
            );
            let failure = v4.expect_err("the write timed out");
            assert_eq!(failure.code, "TIMEOUT_ERROR");
            assert_eq!(
                failure.message,
                "Timeout error: data_write query timed out after 150ms"
            );
            assert!(failure.retryable);
        }
    }
}
