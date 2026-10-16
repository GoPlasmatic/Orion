use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::{Ctx, Function, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::task_context::TaskContext;
use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
use mongodb::bson::Document;
use serde_json::{Map, Value, json};

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, QueryBudget, QueryFailure, acquire_conn, build_entity_registry, decode_failure,
    encode_failure, es_request, es_write_error, is_mongo, require_op_allowed, resolve_params,
    resolve_row_format, send_es, to_connect_error, to_exec_error,
};
use super::db_read::rendered_row_format;
use super::mongo_common::{delete_envelope, empty_insert_envelope, update_envelope};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::{Scope, TemplatedInput};
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::sql_decode::RowFormat;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::engine::HandlerError;
use crate::engine::kit::{self, read};
use crate::query::backend::es::{EsWrite, bulk_ndjson};
use crate::query::backend::mongo::MongoWrite;
use crate::query::write::{ResolvedWrite, WriteOp};
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
const NAME: &str = <DataWriteHandler as ConnectorHandler>::NAME;

/// Audit status for a bulk write that applied some but not all of its items
/// (F28) — HTTP 207 Multi-Status, the code that exists for exactly this. Below
/// 400, so the workflow continues; not 200, so the trace shows it happened.
/// Shared with `mongo_write` (#263), whose bulk inserts classify identically.
pub(super) const PARTIAL_STATUS: u16 = 207;

/// Executes a portable `data_write` — one backend-neutral mutation envelope
/// (`op`/`target`/`values`/`set`/`filter`/`on_conflict`/`returning`) that renders
/// to a native SQL `INSERT`/`UPDATE`/`DELETE`/upsert, a MongoDB write, or an
/// Elasticsearch write — against a SQL, Mongo, or ES connector. The translation
/// — including the write-safety guards (bulk cap, unfiltered mutation; W15) —
/// lives in `src/query/write.rs` and the per-backend renderers; this handler
/// wires it to the connector/pool machinery (ES runs over the HTTP client).
pub struct DataWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    /// Safety bounds (`max_rows` / `allow_unfiltered`) from `[write]`,
    /// enforced inside `resolve_write`.
    pub write_config: crate::config::WriteConfig,
    /// Hard cap on rows a `returning` clause may hand back, from
    /// `query.max_limit` — the same knob and the same reject-never-truncate
    /// rule `db_read` and `mongo_read` apply to a result set.
    ///
    /// `write.max_rows` bounds the rows going *in*; nothing bounded the rows
    /// coming back, so an acknowledged unfiltered `update … returning` read a
    /// whole table into memory.
    pub max_returning: usize,
}

/// The mutation as the task states it, with `params` folded against the
/// message.
///
/// The envelope is carried *unresolved* on purpose: resolving it needs the
/// entity registry, which is built from the connector's own schema guards, so
/// there is nothing to resolve until the connector is in hand.
///
/// Owned by the dataflow-rs 3 handler, which reads it off the authored input
/// per call; the literals borrowed from the compiled config by the
/// dataflow-rs 4 function.
pub struct DataWrite<'a> {
    params: Map<String, Value>,
    /// How a decimal and a binary column in a `returning` row render. SQL
    /// backends only.
    format: RowFormat,
    schema: Option<Cow<'a, Value>>,
    envelope: Cow<'a, Value>,
    database: Option<Cow<'a, str>>,
}

/// What a portable write reaches its backends through, and the bounds it is
/// held to: the handler's or the function's, borrowed.
struct Backends<'a> {
    pool_cache: &'a SqlPoolCache,
    mongo_pool_cache: &'a MongoPoolCache,
    http_client: &'a reqwest::Client,
    write_config: &'a crate::config::WriteConfig,
    max_returning: usize,
}

#[async_trait]
impl ConnectorHandler for DataWriteHandler {
    const NAME: &'static str = "data_write";
    /// Like `data_query`: a `db` or an `es` connector, dispatched on the
    /// variant.
    type Kind = crate::connector::DataBackend;
    type Input = TemplatedInput;
    type Parsed = DataWrite<'static>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        _call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        // W7: the mutation envelope is nested under `write`, mirroring
        // `data_query`'s `query`. Before 1.0 it was flat, sharing a namespace
        // with the handler keys (`connector`, `schema`, `params`, `database`,
        // `output`) — so those five names could never be used by the dialect,
        // and there was no single JSON value that *was* the envelope. The flat
        // form is not accepted: `write` is `required` in the field schema, so
        // the shared validator refuses it at create, update, import, `POST
        // /admin/workflows/validate` and `orion-server lint`, and `orion-server
        // preflight` names any task already stored in that shape.
        let envelope = input
            .get("write")
            .ok_or_else(|| {
                DataflowError::from(query::write::WriteError::Query(
                    query::QueryError::InvalidEnvelope(
                        "missing `write`: the mutation envelope (op/target/values/set/filter/\
                         on_conflict/returning/all) is nested under `write`, alongside the \
                         handler's `connector`/`schema`/`params`/`database`/`output`"
                            .to_string(),
                    ),
                ))
            })?
            .clone();

        Ok(DataWrite {
            // Resolved against the message context (the only point the message
            // touches the mutation); it produces literals, not SQL.
            params: resolve_params(input, <Self as ConnectorHandler>::NAME, ctx),
            format: resolve_row_format(input, <Self as ConnectorHandler>::NAME, ctx)?,
            // Optional inline schema (privileged config): renames, allowlist,
            // and the per-column `writable` flag.
            schema: input.get("schema").cloned().map(Cow::Owned),
            envelope: Cow::Owned(envelope),
            database: input
                .get("database")
                .and_then(Value::as_str)
                .map(|database| Cow::Owned(database.to_string())),
        })
    }

    // No `gate` impl, and this is the one handler for which that is a statement
    // rather than an omission. Which operation to gate is `resolved.op()`, and
    // resolving the envelope needs the entity registry, which is built from the
    // connector's schema guards — so the answer does not exist until after the
    // connector is resolved, which is where `gate` runs from. `run` therefore
    // applies it itself, and the property that matters is kept by construction
    // there: the gate is checked before any pool is touched.

    async fn run(
        &self,
        parsed: Self::Parsed,
        conn: &ConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let backends = Backends {
            pool_cache: &self.pool_cache,
            mongo_pool_cache: &self.mongo_pool_cache,
            http_client: &self.http_client,
            write_config: &self.write_config,
            max_returning: self.max_returning,
        };
        let (result, outcome) = run_write(backends, &parsed, conn, call.connector, None).await?;
        // F28: a partially applied bulk write is a 207, not a success.
        Ok(Produced::with_outcome(result, outcome))
    }
}

/// The write's answer and its outcome, `Success` or F28's partial 207,
/// against a resolved `db` or `es` connector: the envelope resolved, its
/// operation gated, then written. Shared by the dataflow-rs 3 handler and
/// the dataflow-rs 4 function. `run_deadline` is the deadline of the run the
/// call belongs to, which bounds every SQL and MongoDB leg with the
/// connector's `query_timeout_ms` ([`QueryBudget::within`]).
async fn run_write(
    backends: Backends<'_>,
    parsed: &DataWrite<'_>,
    conn: &ConnectorConfig,
    connector: &str,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<(Value, TaskOutcome), HandlerError> {
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
    .map_err(DataflowError::from)?;

    // The gate, before anything is dialled: a connector config can disable
    // individual ops (e.g. delete) regardless of what workflows ask for.
    if let Some(gates) = conn.operation_gates() {
        require_op_allowed(gates, resolved.op().as_str(), connector)?;
    }

    Ok(match conn {
        ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
            // A MongoDB connection string carries no default database, so
            // this is the one branch that requires the task to name one.
            let database = parsed.database.as_deref().ok_or_else(|| {
                DataflowError::Validation(format!("{NAME} requires 'database' field"))
            })?;
            let mw = query::backend::mongo::render_write(&resolved).map_err(DataflowError::from)?;
            let client = backends
                .mongo_pool_cache
                .get_client(connector, db)
                .await
                .map_err(to_connect_error)?;
            // F11: bound the write like the SQL branches bound theirs.
            // The error passes through as itself (`QueryFailure::Classified`):
            // `execute_mongo` has already decided between a partial-bulk
            // 207, a duplicate-key 409 and a backend failure, and
            // stringifying it here threw all three away.
            QueryBudget::within(db.query_timeout_ms, run_deadline)
                .run(NAME, execute_mongo(&client, database, mw))
                .await?
        }
        ConnectorConfig::Db(db) => {
            let dialect: SqlDialect = detect_backend(&db.connection_string)
                .map_err(to_exec_error)?
                .into();
            let (sql, values) = query::backend::sql::render_write(&resolved, dialect)
                .map_err(DataflowError::from)?;
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
        ConnectorConfig::Es(es) => {
            let ew = query::backend::es::render_write(&resolved).map_err(DataflowError::from)?;
            run_es_write(backends.http_client, es, ew).await?
        }
        // `DataBackend` admits `db` and `es` and nothing else; the
        // wrong-type refusal this arm used to write is the wrapper's now.
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
/// documents of `var` holes, as 1.12 folded them. `database` keeps 1.12's
/// reading: a string names the database, anything else is none.
#[derive(Debug, FunctionConfig)]
pub struct DataWriteConfig {
    /// The `db` or `es` connector written through.
    connector: String,
    /// The backend-neutral mutation envelope.
    write: Value,
    /// The named values folded into the envelope's `{"param": …}` nodes.
    params: Option<Interpolated>,
    /// The inline entity schema.
    schema: Option<Value>,
    /// The MongoDB database, read only when it is a string.
    database: Option<Value>,
    /// How a decimal column in a `returning` row renders. SQL backends only.
    numeric_as: Option<Interpolated>,
    /// How a binary column in a `returning` row renders. SQL backends only.
    binary_as: Option<Interpolated>,
    /// Where the write's answer is written.
    #[output(default = "data")]
    output: Output,
}

/// `data_write` as a dataflow-rs 4 function: the write `DataWriteHandler`
/// makes, read in the same order, a partially applied bulk write answering
/// `Outcome::Status(207)`.
pub struct DataWriteFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    /// Safety bounds (`max_rows` / `allow_unfiltered`) from `[write]`.
    pub write_config: crate::config::WriteConfig,
    /// Hard cap on rows a `returning` clause may hand back, from
    /// `query.max_limit`.
    pub max_returning: usize,
}

/// A dataflow-rs 3 outcome of the shared write, as dataflow-rs 4's.
fn outcome(outcome: TaskOutcome) -> Outcome {
    match outcome {
        TaskOutcome::Success => Outcome::Ok,
        TaskOutcome::Status(status) => Outcome::Status(status),
        TaskOutcome::Skip => Outcome::Skip,
        TaskOutcome::Halt => Outcome::Halt,
    }
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
            database: cfg
                .database
                .as_ref()
                .and_then(Value::as_str)
                .map(Cow::Borrowed),
        };
        let run_deadline = ctx.deadline();
        let mut answer = None;
        let done = kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn =
                kit::require_connector::<crate::connector::DataBackend>(&config, &cfg.connector)?;
            let backends = Backends {
                pool_cache: &self.pool_cache,
                mongo_pool_cache: &self.mongo_pool_cache,
                http_client: &self.http_client,
                write_config: &self.write_config,
                max_returning: self.max_returning,
            };
            let (result, done) =
                run_write(backends, &parsed, conn, &cfg.connector, run_deadline).await?;
            answer = Some(result);
            Ok(outcome(done))
        })
        .await?;
        if let Some(answer) = answer {
            ctx.set(
                &output,
                dataflow_rs::datavalue::OwnedDataValue::from(&answer),
            )?;
        }
        // F28: a partially applied bulk write is a 207, not a success.
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
) -> Result<(Value, TaskOutcome), DataflowError> {
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
    // SQL is the atomic member of the three write models, so its status is
    // never `partial`; it carries the key so one check works on every backend.
    out["status"] = json!("ok");
    Ok((out, TaskOutcome::Success))
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
) -> Result<Value, DataflowError>
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

/// Execute a rendered MongoDB write and normalise its result to a small object.
async fn execute_mongo(
    client: &mongodb::Client,
    database: &str,
    mw: MongoWrite,
) -> Result<(Value, TaskOutcome), DataflowError> {
    let db = client.database(database);
    match mw {
        MongoWrite::Insert { collection, docs } => {
            if docs.is_empty() {
                return Ok((empty_insert_envelope(), TaskOutcome::Success));
            }
            let sent = docs.len();
            let coll = db.collection::<Document>(&collection);
            match coll.insert_many(docs).await {
                Ok(res) => {
                    // `inserted_ids` is keyed by input index; return in index order.
                    let mut pairs: Vec<(usize, mongodb::bson::Bson)> =
                        res.inserted_ids.into_iter().collect();
                    pairs.sort_by_key(|(i, _)| *i);
                    let ids: Vec<Option<Value>> = pairs
                        .into_iter()
                        .map(|(_, b)| serde_json::to_value(b).ok())
                        .collect();
                    // Through `bulk_result` like every other bulk answer —
                    // the all-ok case produced the same JSON either way, but
                    // by a second path.
                    bulk_result(query::bulk::BulkOutcome::all_ok(ids), "MongoDB insert")
                }
                // F28: the ordered default means a mid-array failure has
                // already committed everything before it. Recover the indices
                // from the driver's error rather than reporting one opaque
                // message over a half-written collection.
                Err(e) => match mongo_write_errors(&e) {
                    Some(failed) => {
                        let outcome = query::backend::mongo::insert_outcome(sent, &failed);
                        // #297: nothing landed and every item hit a unique
                        // index — the condition SQL answers with 409. A bulk
                        // that partly applied stays a 207, because the envelope
                        // says more there than a status can.
                        if outcome.nothing_applied()
                            && let Some(err) =
                                super::mongo_common::all_duplicate_key(&failed, "MongoDB insert")
                        {
                            return Err(err);
                        }
                        bulk_result(outcome, "MongoDB insert")
                    }
                    None => Err(super::mongo_common::mongo_error(e)),
                },
            }
        }
        MongoWrite::Update {
            collection,
            filter,
            update,
            upsert,
            multi,
        } => {
            let coll = db.collection::<Document>(&collection);
            let res = if multi {
                coll.update_many(filter, update)
                    .await
                    .map_err(super::mongo_common::mongo_error)?
            } else {
                coll.update_one(filter, update)
                    .upsert(upsert)
                    .await
                    .map_err(super::mongo_common::mongo_error)?
            };
            Ok((update_envelope(&res), TaskOutcome::Success))
        }
        MongoWrite::Delete { collection, filter } => {
            let coll = db.collection::<Document>(&collection);
            let res = coll
                .delete_many(filter)
                .await
                .map_err(super::mongo_common::mongo_error)?;
            Ok((delete_envelope(res.deleted_count), TaskOutcome::Success))
        }
    }
}

/// Extract `(index, detail)` pairs from an ordered `insert_many` failure.
///
/// `None` when the error is not a per-item write failure at all — a dropped
/// connection, an auth refusal or a write-concern failure says nothing about
/// which documents landed, so it stays a plain execution error. An *empty*
/// list counts as `None` for the same reason: reporting no failed item for a
/// call that failed would turn the error into a success.
pub(super) fn mongo_write_errors(e: &mongodb::error::Error) -> Option<Vec<(usize, Value)>> {
    let mongodb::error::ErrorKind::InsertMany(bulk) = e.kind.as_ref() else {
        return None;
    };
    let errors = bulk.write_errors.as_ref().filter(|w| !w.is_empty())?;
    Some(
        errors
            .iter()
            .map(|we| {
                (
                    we.index,
                    json!({ "code": we.code, "message": we.message.clone() }),
                )
            })
            .collect(),
    )
}

/// Turn a bulk outcome into the handler's `(result, outcome)` pair (F28).
///
/// A partial bulk is reported as multi-status, not as an error: failing the
/// task would abort the workflow while leaving the applied prefix unnamed,
/// which is precisely the defect. `207` keeps the applied/failed detail on the
/// output path and stamps the audit trail with something other than `200`, so
/// a partial write is visible in traces without being fatal.
///
/// Nothing applied is still a hard error — there is no partial state to
/// describe, and a silent no-op would be worse than a failed workflow.
pub(super) fn bulk_result(
    outcome: query::bulk::BulkOutcome,
    what: &str,
) -> Result<(Value, TaskOutcome), DataflowError> {
    if outcome.nothing_applied() {
        let first = outcome.first_error().cloned().unwrap_or(Value::Null);
        return Err(DataflowError::function_execution(
            format!("{what} failed, no documents were written: {first}"),
            None,
        ));
    }
    let task = if outcome.is_partial() {
        TaskOutcome::Status(PARTIAL_STATUS)
    } else {
        TaskOutcome::Success
    };
    Ok((outcome.to_json(), task))
}

/// Build `{base}/{segments...}?{query...}` with percent-encoded path segments
/// (the document `_id` travels in the path).
fn es_url(base: &str, segments: &[&str], query: &[(&str, &str)]) -> Result<String, DataflowError> {
    let mut url = url::Url::parse(base).map_err(to_exec_error)?;
    url.path_segments_mut()
        .map_err(|_| DataflowError::Validation("es connector url cannot be a base".to_string()))?
        .pop_if_empty()
        .extend(segments);
    for (k, v) in query {
        url.query_pairs_mut().append_pair(k, v);
    }
    Ok(url.into())
}

/// Execute a rendered Elasticsearch write and normalise its result to the same
/// small object the Mongo path returns.
///
/// Every write requests a refresh (`wait_for` doc-level; `true` on the by-query
/// endpoints, which don't support `wait_for`) so a `data_query` later in the same
/// pipeline reads its own writes — parity with SQL/Mongo visibility, at a
/// throughput cost. A per-envelope knob is future work.
async fn run_es_write(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    ew: EsWrite,
) -> Result<(Value, TaskOutcome), DataflowError> {
    match ew {
        EsWrite::BulkInsert { index, docs } => {
            if docs.is_empty() {
                return Ok((
                    json!({ "status": "ok", "inserted": 0, "ids": [] }),
                    TaskOutcome::Success,
                ));
            }
            let sent = docs.len();
            let url = es_url(&es.url, &[&index, "_bulk"], &[("refresh", "wait_for")])?;
            let req = es_request(client, es, reqwest::Method::POST, &url)
                .await?
                .header("Content-Type", "application/x-ndjson")
                .body(bulk_ndjson(&docs));
            let (status, body) = send_es(req, es.max_response_size).await?;
            if !status.is_success() {
                return Err(es_write_error(status, &body));
            }
            // F28: `_bulk` attempts every item independently, so any subset can
            // have landed. Report all of them rather than the first error.
            bulk_result(
                query::backend::es::bulk_outcome(&body, sent),
                "Elasticsearch bulk insert",
            )
        }
        EsWrite::UpdateByQuery { index, body } => {
            let resp = run_by_query(client, es, &index, "_update_by_query", &body).await?;
            Ok((
                json!({ "status": "ok", "matched": resp["total"], "modified": resp["updated"] }),
                TaskOutcome::Success,
            ))
        }
        EsWrite::DeleteByQuery { index, body } => {
            let resp = run_by_query(client, es, &index, "_delete_by_query", &body).await?;
            Ok((
                json!({ "status": "ok", "deleted": resp["deleted"] }),
                TaskOutcome::Success,
            ))
        }
        EsWrite::UpdateDoc { index, id, body } => {
            let url = es_url(
                &es.url,
                &[&index, "_update", &id],
                &[("refresh", "wait_for")],
            )?;
            let req = es_request(client, es, reqwest::Method::POST, &url)
                .await?
                .json(&body);
            let (status, resp) = send_es(req, es.max_response_size).await?;
            if !status.is_success() {
                return Err(es_write_error(status, &resp));
            }
            let out = match resp.get("result").and_then(|r| r.as_str()) {
                Some("created") => {
                    json!({ "status": "ok", "matched": 0, "modified": 0, "upserted_id": id })
                }
                Some("noop") => json!({ "status": "ok", "matched": 1, "modified": 0 }),
                _ => json!({ "status": "ok", "matched": 1, "modified": 1 }),
            };
            Ok((out, TaskOutcome::Success))
        }
        EsWrite::CreateDoc { index, id, doc } => {
            let url = es_url(
                &es.url,
                &[&index, "_doc", &id],
                &[("op_type", "create"), ("refresh", "wait_for")],
            )?;
            let req = es_request(client, es, reqwest::Method::PUT, &url)
                .await?
                .json(&doc);
            let (status, resp) = send_es(req, es.max_response_size).await?;
            if status == reqwest::StatusCode::CONFLICT {
                // The document exists — `action: "nothing"` semantics.
                return Ok((
                    json!({ "status": "ok", "matched": 1, "modified": 0 }),
                    TaskOutcome::Success,
                ));
            }
            if !status.is_success() {
                return Err(es_write_error(status, &resp));
            }
            Ok((
                json!({ "status": "ok", "matched": 0, "modified": 0, "upserted_id": id }),
                TaskOutcome::Success,
            ))
        }
    }
}

/// POST an `_update_by_query` / `_delete_by_query` body (with `refresh=true`) and
/// surface failures/version conflicts as errors — the endpoints run with the
/// default `conflicts=abort`, matching parity-or-error.
async fn run_by_query(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    index: &str,
    endpoint: &str,
    body: &Value,
) -> Result<Value, DataflowError> {
    let url = es_url(&es.url, &[index, endpoint], &[("refresh", "true")])?;
    let req = es_request(client, es, reqwest::Method::POST, &url)
        .await?
        .json(body);
    let (status, resp) = send_es(req, es.max_response_size).await?;
    if !status.is_success() {
        return Err(es_write_error(status, &resp));
    }
    let failed = resp
        .get("failures")
        .and_then(|f| f.as_array())
        .is_some_and(|f| !f.is_empty())
        || resp
            .get("version_conflicts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0;
    if failed {
        return Err(DataflowError::function_execution(
            format!("Elasticsearch {endpoint} had failures: {resp}"),
            None,
        ));
    }
    Ok(resp)
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
        description: "Name of the db (SQL/MongoDB) or es (Elasticsearch) connector to write to.",
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
        // `POST /admin/workflows/validate` and `orion-server lint`, rather than
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
        name: "database",
        description: "MongoDB database name. Optional here because the same task shape is \
                      valid against SQL and Elasticsearch, which need no database key; \
                      required — and checked at workflow activation — once the referenced \
                      connector is a MongoDB one (F52).",
        kind: FieldKind::String,
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
        description: "Logical entity to write to (schema-resolved to a table/collection).",
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
    use crate::query::bulk::{BulkOutcome, ItemOutcome};

    /// The three-way classification F28 turns a doc-store bulk into. It is the
    /// most consequential behaviour change in the wave — a partial write no
    /// longer aborts the workflow — and it is a pure function over
    /// `BulkOutcome`, so it is checked here rather than only through a
    /// container.
    #[test]
    fn a_clean_bulk_is_a_plain_success() {
        let (out, task) = bulk_result(
            BulkOutcome::all_ok(vec![Some(json!("a")), Some(json!("b"))]),
            "MongoDB insert",
        )
        .expect("a clean bulk is not an error");
        assert_eq!(task, TaskOutcome::Success);
        assert_eq!(out["status"], "ok", "{out}");
        assert_eq!(out["inserted"], 2, "{out}");
    }

    /// A partial write reports 207 and keeps going: erroring would abort the
    /// workflow while leaving the applied prefix unnamed, which is the defect.
    #[test]
    fn a_partial_bulk_is_multi_status_not_an_error() {
        let (out, task) = bulk_result(
            BulkOutcome {
                items: vec![
                    ItemOutcome::ok(0, Some(json!("a"))),
                    ItemOutcome::error(1, json!({ "code": 11000 })),
                    ItemOutcome::skipped(2),
                ],
            },
            "MongoDB insert",
        )
        .expect("a partial write must not fail the task");
        assert_eq!(
            task,
            TaskOutcome::Status(PARTIAL_STATUS),
            "a partial write must be visible in the audit trail as 207"
        );
        assert_eq!(out["status"], "partial", "{out}");
        assert_eq!(out["items"][1]["index"], 1, "{out}");
    }

    /// Nothing landed, so there is no partial state to describe — and a
    /// silent no-op would be worse than a failed workflow. The message names
    /// the first failure so the trace says *why*.
    #[test]
    fn a_bulk_that_applied_nothing_is_a_hard_error() {
        let err = bulk_result(
            BulkOutcome {
                items: vec![
                    ItemOutcome::error(0, json!({ "message": "duplicate key" })),
                    ItemOutcome::skipped(1),
                ],
            },
            "MongoDB insert",
        )
        .expect_err("a bulk that wrote nothing is a failure");
        let msg = err.to_string();
        assert!(msg.contains("MongoDB insert"), "{msg}");
        assert!(msg.contains("duplicate key"), "must name why: {msg}");
    }

    // -- dataflow-rs 4: the config, and twins of the handler over in-memory
    // SQLite and a mock Elasticsearch node --

    use std::time::Duration;

    use crate::engine::functions::twin::{self, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use dataflow_rs::run::ErrorKind;

    const SEED: &[&str] = &[
        "CREATE TABLE users (id TEXT PRIMARY KEY, status TEXT, avatar BLOB)",
        "INSERT INTO users VALUES ('u1', 'active', X'00FF')",
    ];

    fn handler(
        pool_cache: Arc<SqlPoolCache>,
        registry: Arc<ConnectorRegistry>,
    ) -> dataflow_rs_v3::BoxedFunctionHandler {
        Box::new(super::super::connector_handler::Connector(
            DataWriteHandler {
                pool_cache,
                mongo_pool_cache: Arc::new(MongoPoolCache::new(8)),
                http_client: reqwest::Client::new(),
                registry,
                write_config: crate::config::WriteConfig::default(),
                max_returning: 1000,
            },
        ))
    }

    fn function(
        pool_cache: Arc<SqlPoolCache>,
        registry: Arc<ConnectorRegistry>,
    ) -> DataWriteFunction {
        DataWriteFunction {
            pool_cache,
            mongo_pool_cache: Arc::new(MongoPoolCache::new(8)),
            http_client: reqwest::Client::new(),
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

    /// One task through each engine, each against a database of its own
    /// seeded with [`SEED`], its connector configured with `config`; and
    /// each database's rows after it.
    async fn both(input: Value, data: Value, config: Value) -> ((Seen, Value), (Seen, Value)) {
        let mut sides = Vec::new();
        for v4 in [false, true] {
            let db = twin::sqlite(config.clone(), SEED).await;
            let (pool_cache, registry) = (db.pool_cache.clone(), db.registry.clone());
            let seen = if v4 {
                twin::v4(
                    NAME,
                    function(pool_cache, registry),
                    input.clone(),
                    data.clone(),
                )
                .await
            } else {
                twin::v3(
                    NAME,
                    handler(pool_cache, registry),
                    input.clone(),
                    data.clone(),
                )
                .await
            };
            sides.push((seen, users(&db).await));
        }
        let v4 = sides.pop().expect("v4");
        (sides.pop().expect("v3"), v4)
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
            &crate::engine::kit::test_exprs(),
            json!({"connector": "db", "write": envelope.clone(), "params": params.clone(),
                   "database": ["not", "a", "string"]}),
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
            testing::config_with::<DataWriteConfig>(&crate::engine::kit::test_exprs(), input)
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
    /// answered at `output`, each database ending the same.
    #[tokio::test]
    async fn a_write_answers_as_on_v3() {
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
            let (v3, v4) = both(input.clone(), data.clone(), json!({})).await;
            assert_eq!(v4, v3, "{input}");
            assert_eq!(v4.0.expect("written")["w"]["status"], "ok", "{input}");
        }
        // `binary_as` folds into a `returning` row's rendering.
        let mut input = write(json!({
            "op": "delete", "target": "users",
            "filter": {"==": [{"field": "id"}, "u1"]}, "returning": ["avatar"]
        }));
        input["binary_as"] = json!({"var": "data.enc"});
        let (v3, v4) = both(input, json!({"enc": "base64"}), json!({})).await;
        assert_eq!(v4, v3);
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
            let (v3, v4) = both(input.clone(), json!({"n": 1}), config).await;
            assert_eq!(v4, v3, "{input}");
            assert!(v4.0.is_err(), "{input}");
            assert_eq!(v4.1, json!([["u1", "active"]]), "{input}");
        }
    }

    /// A mock Elasticsearch node answering `_bulk` with `items`, counting
    /// the requests it sees.
    async fn es_node(items: Value) -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let app = axum::Router::new().fallback(move || {
            let counter = Arc::clone(&counter);
            let items = items.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"took": 1, "errors": true, "items": items}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (addr, seen)
    }

    /// F28 on dataflow-rs 4: a bulk insert that applied some of its items
    /// answers `Outcome::Status(207)` with the applied and failed items at
    /// `output`, where 1.12 recorded status 207; one that applied all is a
    /// plain success, and one that applied none a hard error, as on 1.12.
    #[tokio::test]
    async fn a_partial_bulk_write_is_outcome_status_207() {
        let ok = |id: &str| json!({"index": {"_id": id, "status": 201, "result": "created"}});
        let failed = json!({"index": {"_id": "b", "status": 400,
                             "error": {"type": "mapper_parsing_exception", "reason": "bad"}}});
        let input = write(json!({"op": "insert", "target": "events", "values": [
            {"id": "a", "n": 1}, {"id": "b", "n": "x"}
        ]}));
        for (items, outcome, status) in [
            (
                json!([ok("a"), failed.clone()]),
                Some(Outcome::Status(207)),
                Some(207),
            ),
            (json!([ok("a"), ok("b")]), Some(Outcome::Ok), Some(200)),
            (json!([failed.clone(), failed]), None, None),
        ] {
            let mut sides = Vec::new();
            for v4 in [false, true] {
                let (addr, seen) = es_node(items.clone()).await;
                let connector = json!({
                    "type": "es", "url": format!("http://{addr}"), "allow_private_urls": true
                });
                let registry = twin::connectors(&[("db", connector)]).await;
                let pool_cache = Arc::new(SqlPoolCache::new(8));
                if v4 {
                    let called = testing::call(function(pool_cache, registry), input.clone())
                        .exprs(crate::engine::kit::test_exprs())
                        .name(NAME)
                        .await;
                    let done = called.outcome.as_ref().ok().copied();
                    assert_eq!(done, outcome, "{items}");
                    sides.push((
                        twin::seen(&called),
                        seen.load(std::sync::atomic::Ordering::SeqCst),
                    ));
                } else {
                    let (v3, audit) = twin::v3_status(
                        NAME,
                        handler(pool_cache, registry),
                        input.clone(),
                        Value::Null,
                    )
                    .await;
                    if v3.is_ok() {
                        assert_eq!(audit, status, "{items}");
                    }
                    sides.push((v3, seen.load(std::sync::atomic::Ordering::SeqCst)));
                }
            }
            assert_eq!(sides[1], sides[0], "{items}");
            assert_eq!(sides[1].1, 1, "one bulk request");
            match outcome {
                Some(Outcome::Status(207)) => {
                    let answer = sides[1].0.clone().expect("a partial write is not an error");
                    assert_eq!(answer["w"]["status"], "partial", "{answer}");
                }
                Some(_) => assert_eq!(sides[1].0.clone().expect("written")["w"]["status"], "ok"),
                None => assert!(sides[1].0.is_err(), "nothing applied"),
            }
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
        .exprs(crate::engine::kit::test_exprs())
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
            let held = db.pool.acquire().await.expect("the one connection");
            let v3 = twin::v3(
                NAME,
                handler(db.pool_cache.clone(), db.registry.clone()),
                input.clone(),
                Value::Null,
            )
            .await;
            drop(held);
            let db = twin::sqlite(config.clone(), SEED).await;
            let _held = db.pool.acquire().await.expect("the one connection");
            let called = testing::call(
                function(db.pool_cache.clone(), db.registry.clone()),
                input.clone(),
            )
            .exprs(crate::engine::kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
            .await;
            let v4 = twin::seen(&called);
            assert_eq!(v4, v3, "{input}");
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
