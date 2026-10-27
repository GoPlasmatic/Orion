use super::definition::{Definition, FieldOverlay, OrionFunction};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use serde_json::{Map, Value};
// sqlx 0.9 refuses a non-`'static` query string unless it is wrapped, so that
// building SQL by concatenation has to be an explicit, greppable decision. Every
// string wrapped in this module is emitted by `query::backend::sql` from a
// parsed IR — table and column identifiers are quoted by the dialect and every
// author-supplied *value* travels as a bind parameter in the `args` beside it,
// never spliced into the text. The raw-SQL escape hatch is `db_read`/`db_write`,
// not this path.
use sqlx::AssertSqlSafe;

use super::connector_helpers::{
    QueryBudget, QueryFailure, acquire_conn, build_entity_registry, decode_failure, encode_failure,
    require_op_allowed, to_connect_error, to_exec_error,
};
use super::db_read::rendered_row_format;
use super::schema::{FieldKind, RetrySafety};
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::sql_decode::RowFormat;
use crate::connector::{ConnectorConfig, ConnectorRegistry};
use crate::kit::{self, read};
use crate::query::{self, GroupKey, SqlDialect};
use crate::storage::detect_backend;
use crate::{ErrorClass, HandlerError};

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
pub(crate) const NAME: &str = "data_query";

/// Parent keys per `include` child query.
///
/// The keys go into one `IN (…)`, a bind parameter each, and a driver has a
/// ceiling on those: PostgreSQL's protocol stops at 65535 per statement, and
/// SQLite builds before 3.32 at 999. Neither is reachable at the default
/// `query.max_limit` of 1000 — which is why this is the batch size, so a
/// default deployment issues exactly the one query it always did — but
/// `max_limit` is a documented knob, and raising it should not turn an
/// `include` into a driver error naming neither the knob nor the relation.
const MAX_INCLUDE_KEYS_PER_QUERY: usize = 1000;

/// A portable query, read from the task with only its `params` folded against
/// the message.
///
/// The literals are borrowed from the compiled config.
pub struct DataQuery<'a> {
    query: Cow<'a, Value>,
    params: Map<String, Value>,
    schema: Option<Cow<'a, Value>>,
    /// How a decimal and a binary column render.
    format: RowFormat,
}

/// What a portable query reaches its backend through: the handler's or the
/// function's dependencies, borrowed.
struct Backends<'a> {
    pool_cache: &'a SqlPoolCache,
    limits: &'a crate::config::QueryConfig,
}

/// The connector's gate for a read. Per-connector operation gates: reads
/// can be disabled too (e.g. a write-only audit sink).
fn gate(conn: &ConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    if let Some(gates) = conn.operation_gates() {
        require_op_allowed(gates, "read", connector)?;
    }
    Ok(())
}

/// The query's answer, rows or a count, against a resolved and gated `db`
/// connector. `run_deadline` is the deadline of the run the call belongs to,
/// which bounds every SQL leg with the connector's `query_timeout_ms`
/// ([`QueryBudget::within`]).
async fn run_query(
    backends: Backends<'_>,
    parsed: &DataQuery<'_>,
    conn: &ConnectorConfig,
    connector: &str,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<Value, HandlerError> {
    // F24: with no schema the registry rejects rather than passing every
    // name through, and the connector's own guards apply on top.
    let registry = build_entity_registry(parsed.schema.as_deref(), conn, connector)?;
    let query = &*parsed.query;
    let params = &parsed.params;
    let limits = backends.limits;

    let result = match conn {
        ConnectorConfig::Db(db) => {
            // SQL: dialect from the connection-string scheme, the same
            // source the pool is built from, so the rendered SQL always
            // matches the driver that runs it.
            let dialect: SqlDialect = detect_backend(&db.connection_string)
                .map_err(to_exec_error)?
                .into();
            let plan = query::plan_sql(query, params, &registry, dialect, limits)
                .map_err(HandlerError::from)?;
            let pool = backends
                .pool_cache
                .get_pool(connector, db)
                .await
                .map_err(to_connect_error)?;
            if plan.count {
                return run_sql_count(&pool, &plan, dialect, db.query_timeout_ms, run_deadline)
                    .await;
            }
            run_sql_with_includes(
                &pool,
                &plan,
                dialect,
                QueryBudget::within(db.query_timeout_ms, run_deadline),
                parsed.format,
                limits.max_result_bytes,
            )
            .await?
        }
        // `DataBackend` admits `db` and nothing else, and it produced the
        // wrong-type refusal, so this handler writes none of its own.
        other => unreachable!(
            "DataBackend admitted a '{}' connector",
            other.connector_type()
        ),
    };

    Ok(result)
}

/// Whether a query envelope reads rows, rather than counting them: anything
/// but `"count": true`, which answers `{"count": n}`.
fn reads_rows(query: &Value) -> bool {
    query.get("count").and_then(Value::as_bool) != Some(true)
}

/// I185's Defect: a row read's result is an array, which no context root
/// takes (break 21), so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`data_query` writes an array of rows, and a context root takes \
    only an object (break 21): set `output` to a path below it, such as `data.rows`";

/// `data_query`'s input on dataflow-rs 4, in its field table's order (I179).
/// The connector and the envelope are required at load (I183). The envelope
/// and the schema are literal documents, `{"param": …}` holes and all, for
/// Orion's own dialect to fold; `params` and the rendering choices are
/// documents of `var` holes, as 1.12 folded them.
#[derive(Debug, FunctionConfig)]
pub struct DataQueryConfig {
    /// Name of the db (SQL) connector to query.
    connector: String,
    /// Backend-neutral query envelope:
    /// source/filter/fields/sort/limit/skip/after/include/count. An include selection is
    /// {fields, sort, limit}; `sort` is required because the per-parent page is cut in the
    /// database. "after" is a keyset cursor — the previous page's last row, one value per sort
    /// key — which pages without an offset and so is not bounded by query.max_skip. "count":
    /// true answers {"count": n} instead of rows, and refuses the keys that shape a row set.
    query: Value,
    /// Object of named values folded into the filter's {"param": ..} nodes. A value of {"var":
    /// "path"} is read from the message context.
    params: Option<Interpolated>,
    /// Inline entity schema (renames, type hints, allowlist, relations) enabling some/all/none
    /// and typed coercion. Undeclared entities and columns are rejected, so a query without one
    /// reaches nothing; pass {"unmapped": "identity"} for pre-1.0 pass-through.
    schema: Option<Value>,
    /// How an arbitrary-precision decimal column is rendered: "number" (default) or "string". A
    /// number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a
    /// string keeps every digit, which is what a money column needs. SQL backends only.
    numeric_as: Option<Interpolated>,
    /// How a binary column is rendered: "auto" (default), "hex", "base64" or "text". Auto reads
    /// the bytes as text when they are valid UTF-8 and as hex when they are not, so its result
    /// shape depends on the data; name an encoding for a column that is genuinely binary. SQL
    /// backends only.
    binary_as: Option<Interpolated>,
    /// Dotted path in the message where rows are written. Defaults to "data".
    #[output(default = "data")]
    output: Output,
}

/// `data_query` as a dataflow-rs 4 function: the query `DataQueryHandler`
/// runs, read in the same order.
pub struct DataQueryFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Page bounds (`default_limit` / `max_limit` / `max_skip`) from `[query]`.
    pub limits: crate::config::QueryConfig,
}

impl OrionFunction for DataQueryFunction {
    type Connector = crate::connector::DataBackend;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Run a backend-neutral query (filter + envelope) against a SQL connector.",
        category: "connector",
        retry_safety: RetrySafety::Read,
        fields: &[
            FieldOverlay {
                name: "query",
                kind: Some(FieldKind::Object),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "params",
                kind: Some(FieldKind::Object),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "schema",
                kind: Some(FieldKind::Object),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "numeric_as",
                kind: Some(FieldKind::String),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "binary_as",
                kind: Some(FieldKind::String),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
        ],
        deny_unknown: false,
        stub_target: Some("connector"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            pool_cache: deps.sql_pool_cache.clone(),
            registry: deps.registry.clone(),
            limits: deps.query_config.clone(),
        }
    }
}

impl Function for DataQueryFunction {
    type Config = DataQueryConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    /// I185: a row read whose `output` is a literal context root, the
    /// default `data` included, is a Defect at `output`. A count answers an
    /// object, which a root takes.
    fn prepare(&self, cfg: &mut DataQueryConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        if reads_rows(&cfg.query) {
            kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
        } else {
            Ok(())
        }
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DataQueryConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse`'s order: `params`, the
        // rendering choices, the literals.
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
        let params = cfg
            .params
            .as_ref()
            .map(|doc| kit::rendered(ctx, doc))
            .transpose()?;
        let parsed = DataQuery {
            query: Cow::Borrowed(&cfg.query),
            params: read::params(params),
            format: rendered_row_format(
                ctx,
                cfg.numeric_as.as_ref(),
                cfg.binary_as.as_ref(),
                NAME,
            )?,
            schema: cfg.schema.as_ref().map(Cow::Borrowed),
        };
        let run_deadline = ctx.deadline();
        let mut answer = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let conn = kit::require_connector::<<Self as OrionFunction>::Connector>(
                &config,
                &cfg.connector,
            )?;
            gate(conn, &cfg.connector)?;
            let backends = Backends {
                pool_cache: &self.pool_cache,
                limits: &self.limits,
            };
            answer = Some(run_query(backends, &parsed, conn, &cfg.connector, run_deadline).await?);
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

/// The shape a `"count": true` envelope is answered with: one object, one key.
fn count_result(n: u64) -> Value {
    serde_json::json!({ "count": n })
}

/// Execute the rendered `COUNT(*)`: one row, one column, read by the alias the
/// renderer projected it under so the three dialects' default names for the
/// expression never matter.
async fn run_sql_count(
    pool: &crate::connector::pool_cache::SqlPool,
    plan: &query::SqlPlan,
    dialect: SqlDialect,
    timeout_ms: Option<u64>,
    run_deadline: Option<dataflow_rs::run::Instant>,
) -> Result<Value, HandlerError> {
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    // Two legs now — acquire, then the statement — so they share one budget
    // rather than each getting the connector's whole `query_timeout_ms`.
    let budget = QueryBudget::within(timeout_ms, run_deadline);
    let rows: Vec<Value> = crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, _bind, typed_args, _write_result => {
            let scalars = crate::connector::sql_encode::scalars_from_sea(&values.0);
            let mut conn = acquire_conn(&budget, NAME, p).await?;
            let bound = budget
                .run(NAME, async {
                    typed_args(&mut conn, &sql, scalars.as_deref())
                        .await
                        .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                })
                .await?;
            // Converted up front, with the pool pinning which driver's
            // arguments these are; the move itself is free.
            let fallback = crate::connector::sql_encode::sea_args_for(p, values);
            let q = match bound {
                crate::connector::sql_encode::Bound::Typed(args) => {
                    sqlx::query_with(AssertSqlSafe(sql.as_str()), args)
                }
                crate::connector::sql_encode::Bound::Fallback { cache } => {
                    sqlx::query_with(AssertSqlSafe(sql.as_str()), fallback)
                        .persistent(cache)
                }
            };
            let rows = budget.run(NAME, q.fetch_all(&mut *conn)).await?;
            rows_to_json(&rows, crate::connector::sql_decode::RowFormat::default())
                .map_err(|e| decode_failure(NAME, e))?
        }
    );
    let n = rows
        .first()
        .and_then(|r| r.get(query::backend::sql::COUNT_COLUMN))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            HandlerError::new(
                ErrorClass::Backend,
                format!("{NAME}: the count query returned no count"),
            )
        })?;
    Ok(count_result(n))
}

/// Execute the main SQL query, then hydrate each `include` with a per-relation
/// child query (`WHERE fk IN (parent keys)`), grouping children back to their
/// parents in memory. One extra query per include relation; no per-dialect JSON
/// functions needed.
///
/// The per-parent page is cut **in the child query** (`ROW_NUMBER() OVER
/// (PARTITION BY fk ORDER BY …)`, see
/// [`query::backend::sql::build_include_select`]), not here: fetching every
/// child of every parent on the page and truncating afterwards would
/// materialise ten million rows for a thousand parents with ten thousand
/// children each, to return five apiece — in an order nothing defined (F27).
///
/// Grouping is by [`query::GroupKey`], not by the key's JSON text, so a parent
/// key and a child foreign key that the driver rendered differently (`"7"` vs
/// `7`) still join (W14).
///
/// Every round trip — the main query and one child query per include relation —
/// shares a single [`QueryBudget`]. A `timed_query` per round trip would be a
/// fresh budget each, so a query with three includes could run for four
/// times the `query_timeout_ms` its connector was configured with. That is the
/// hazard `QueryBudget` exists to close on the write path, and the read path
/// with includes is the other place a logical operation spans round trips.
async fn run_sql_with_includes(
    pool: &crate::connector::pool_cache::SqlPool,
    plan: &query::SqlPlan,
    dialect: SqlDialect,
    budget: QueryBudget,
    format: crate::connector::sql_decode::RowFormat,
    max_result_bytes: usize,
) -> Result<Value, HandlerError> {
    let mut total_bytes = 0usize;
    let check_bytes = |rows: &[Value], total: &mut usize| -> Result<(), HandlerError> {
        if max_result_bytes == 0 {
            return Ok(());
        }
        for row in rows {
            *total =
                total.saturating_add(crate::connector::sql_decode::approx_json_value_bytes(row));
            if *total > max_result_bytes {
                return Err(HandlerError::new(
                    ErrorClass::CallerInput,
                    format!(
                        "{NAME} result exceeds query.max_result_bytes ({max_result_bytes} bytes) \
                     — select fewer fields, reduce limit, or raise the cap"
                    ),
                ));
            }
        }
        Ok(())
    };
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    // `SqlxValues` implements `IntoArguments` for all three drivers as well as
    // `Any`, so the builder above needs no change — only the execute site
    // dispatches (#309).
    let mut parents: Vec<Value> = crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, _bind, typed_args, _write_result => {
            // One connection per leg, not one for the handler: a prepared
            // statement's parameter types are cached per connection, so a
            // prepare and its execute must share one — but the legs below are
            // separate statements and need nothing from each other. Holding a
            // single connection across all of them would raise occupancy
            // against `max_connections` for no gain.
            let scalars = crate::connector::sql_encode::scalars_from_sea(&values.0);
            let mut conn = acquire_conn(&budget, NAME, p).await?;
            let bound = budget
                .run(NAME, async {
                    typed_args(&mut conn, &sql, scalars.as_deref())
                        .await
                        .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                })
                .await?;
            // Converted up front, with the pool pinning which driver's
            // arguments these are; the move itself is free.
            let fallback = crate::connector::sql_encode::sea_args_for(p, values);
            let q = match bound {
                crate::connector::sql_encode::Bound::Typed(args) => {
                    sqlx::query_with(AssertSqlSafe(sql.as_str()), args)
                }
                crate::connector::sql_encode::Bound::Fallback { cache } => {
                    sqlx::query_with(AssertSqlSafe(sql.as_str()), fallback)
                        .persistent(cache)
                }
            };
            let rows = budget.run(NAME, q.fetch_all(&mut *conn)).await?;
            rows_to_json(&rows, format).map_err(|e| decode_failure(NAME, e))?
        }
    );
    check_bytes(&parents, &mut total_bytes)?;

    for inc in &plan.includes {
        // Distinct, non-null parent keys to fetch children for.
        let mut seen = HashSet::new();
        let mut keys = Vec::new();
        for p in &parents {
            if let Some(k) = p.get(&inc.local)
                && let Some(gk) = GroupKey::from_json(k)
                && let Some(sv) = query::backend::sql::json_key_to_sea(k)
                && seen.insert(gk)
            {
                keys.push(sv);
            }
        }

        // Fetch children and group them by their foreign-key value. `strip` is
        // the part of the child projection the caller did not ask for: the
        // grouping key, and any column projected only because the outer
        // `ORDER BY` names it (see `IncludePlan::projection`).
        let strip = inc.strip();
        let mut groups: HashMap<GroupKey, Vec<Value>> = HashMap::new();
        // One child query per batch of parent keys. A parent's key lands in
        // exactly one batch and the per-parent page is cut inside the child
        // query, so batching changes no result — only how many bind parameters
        // one statement carries.
        for chunk in keys.chunks(MAX_INCLUDE_KEYS_PER_QUERY) {
            let (csql, cvalues) = query::backend::sql::build_include_select(inc, chunk, dialect);
            let children: Vec<Value> = crate::connector::pool_cache::dispatch_sql_pool!(
                pool, p, rows_to_json, _bind, typed_args, _write_result => {
                    let cscalars = crate::connector::sql_encode::scalars_from_sea(&cvalues.0);
                    let mut conn = acquire_conn(&budget, NAME, p).await?;
                    let bound = budget
                        .run(NAME, async {
                            typed_args(&mut conn, &csql, cscalars.as_deref())
                                .await
                                .map_err(|e| QueryFailure::Classified(encode_failure(NAME, e)))
                        })
                        .await?;
                    // Converted up front, with the pool pinning which driver's
                    // arguments these are; the move itself is free.
                    let fallback = crate::connector::sql_encode::sea_args_for(p, cvalues);
                    let q = match bound {
                        crate::connector::sql_encode::Bound::Typed(args) => {
                            sqlx::query_with(AssertSqlSafe(csql.as_str()), args)
                        }
                        crate::connector::sql_encode::Bound::Fallback { cache } => {
                            sqlx::query_with(AssertSqlSafe(csql.as_str()), fallback)
                                .persistent(cache)
                        }
                    };
                    let crows = budget.run(NAME, q.fetch_all(&mut *conn)).await?;
                    rows_to_json(&crows, format).map_err(|e| decode_failure(NAME, e))?
                }
            );
            check_bytes(&children, &mut total_bytes)?;
            for mut child in children {
                let Some(fk) = child.get(&inc.foreign).and_then(GroupKey::from_json) else {
                    continue;
                };
                if let Value::Object(m) = &mut child {
                    // The window's rank column exists only to cut the page; with
                    // no projection it rides along in the child's `SELECT *`.
                    m.remove(query::backend::sql::INCLUDE_RANK_COLUMN);
                    for s in &strip {
                        m.remove(s);
                    }
                }
                groups.entry(fk).or_default().push(child);
            }
        }

        // Attach the child list to each parent under the relation name. The list
        // is already bounded and ordered by the child query.
        for p in &mut parents {
            let kids = p
                .get(&inc.local)
                .and_then(GroupKey::from_json)
                .and_then(|k| groups.get(&k).cloned())
                .unwrap_or_default();
            if let Value::Object(m) = p {
                m.insert(inc.field.clone(), Value::Array(kids));
            }
        }
    }

    // Remove parent columns that were added only to group children.
    if !plan.strip.is_empty() {
        for p in &mut parents {
            if let Value::Object(m) = p {
                for s in &plan.strip {
                    m.remove(s);
                }
            }
        }
    }

    Ok(Value::Array(parents))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::functions::twin::{self, Seen};
    use dataflow_rs::ErrorKind;
    use dataflow_rs::function::testing;
    use dataflow_rs::program::{IssueCode, Severity};
    use serde_json::json;

    const SEED: &[&str] = &[
        "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT, age INTEGER, avatar BLOB)",
        "INSERT INTO users VALUES ('u1', 'Alice', 15, X'00FF'), ('u2', 'Bob', 20, X'6869'), \
         ('u3', 'Carol', 30, NULL), ('u4', 'Dave', 40, NULL)",
    ];

    fn function(
        pool_cache: Arc<SqlPoolCache>,
        registry: Arc<ConnectorRegistry>,
    ) -> DataQueryFunction {
        DataQueryFunction {
            pool_cache,
            registry,
            limits: crate::config::QueryConfig::default(),
        }
    }

    /// One task against a database of its own seeded with [`SEED`], its
    /// connector configured with `config`.
    async fn run(input: Value, data: Value, config: Value) -> Seen {
        let db = twin::sqlite(config, SEED).await;
        twin::v4(NAME, function(db.pool_cache, db.registry), input, data).await
    }

    /// A `data_query` task over `users`, every column readable.
    fn query(envelope: Value) -> Value {
        json!({
            "connector": "db", "query": envelope,
            "schema": {"unmapped": "identity"}, "output": "data.rows"
        })
    }

    fn rejected(input: Value) -> Vec<(String, IssueCode)> {
        testing::config_with::<DataQueryConfig>(&kit::test_exprs(), input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    }

    /// Declaration order is the field table's (I179). The envelope and the
    /// schema are literal documents, `{"param": …}` holes, `var` nodes and
    /// `$` keys as authored, for Orion's dialect to read; `params` is a
    /// document of `var` holes, its `$` keys kept; `connector` and `query`
    /// are required (I183); an unknown key is refused (break 2a), the
    /// removed `database` field included.
    #[test]
    fn the_config_compiles_and_checks_its_inputs() {
        let envelope = json!({
            "source": "users",
            "filter": {">": [{"field": "age"}, {"param": "min"}], "$note": {"var": "data.x"}},
            "sort": [{"age": "asc"}]
        });
        let params = json!({"min": {"var": "data.min"}, "$raw": {"var": ["data.raw", 0]}});
        let schema = json!({"unmapped": "identity", "$id": "s"});
        let cfg = testing::config_with::<DataQueryConfig>(
            &kit::test_exprs(),
            json!({
                "connector": "db", "query": envelope.clone(), "params": params.clone(),
                "schema": schema.clone(), "numeric_as": "string"
            }),
        )
        .expect("compiles");
        assert_eq!(cfg.query, envelope, "the envelope is literal");
        assert_eq!(cfg.schema.as_ref(), Some(&schema));
        let doc = cfg.params.as_ref().expect("set");
        assert_eq!(doc.raw(), &params);
        let holes: Vec<String> = doc.holes().map(ToString::to_string).collect();
        assert_eq!(holes, ["data.min", "data.raw"]);
        let output = cfg.output.constant().map(ToString::to_string);
        assert_eq!(output.as_deref(), Some("data"));

        assert_eq!(
            rejected(json!({"connector": "db"})),
            [("query".to_string(), IssueCode::InputParse)]
        );
        assert_eq!(
            rejected(json!({"query": {"source": "users"}})),
            [("connector".to_string(), IssueCode::InputParse)]
        );
        for key in ["collection", "database"] {
            let mut input = json!({"connector": "db", "query": {}});
            input[key] = json!("c");
            let issues = rejected(input);
            assert_eq!(issues.len(), 1, "{issues:?}");
            assert_eq!(issues[0].0, key);
        }
    }

    /// I185: a row read's result is an array, so a literal root `output`,
    /// the default `data` included, is a Defect at `output`, from `prepare`;
    /// a count answers an object, which a root takes, as 1.12 wrote it.
    #[tokio::test]
    async fn a_row_read_at_a_root_output_is_i185s_defect() {
        for (envelope, output, defect) in [
            (json!({"source": "users"}), None, true),
            (
                json!({"source": "users", "count": false}),
                Some(json!("temp_data")),
                true,
            ),
            (json!({"source": "users"}), Some(json!("data.rows")), false),
            (
                json!({"source": "users"}),
                Some(json!({"cat": ["data", ".rows"]})),
                false,
            ),
            (json!({"source": "users", "count": true}), None, false),
            (
                json!({"source": "users", "count": true}),
                Some(json!("data")),
                false,
            ),
        ] {
            let mut input = json!({
                "connector": "db", "query": envelope, "schema": {"unmapped": "identity"}
            });
            if let Some(output) = &output {
                input["output"] = output.clone();
            }
            let db = twin::sqlite(json!({}), SEED).await;
            let called = testing::call(function(db.pool_cache, db.registry), input.clone())
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
                    "{input}"
                );
                let error = called.outcome.expect_err("an array at a root is refused");
                assert_eq!(error.code(), "CONFIG_ERROR", "{input}");
            } else {
                assert!(defects.is_empty(), "{input}: {defects:?}");
                // The same answer, where 1.12 wrote it.
                twin::pinned(&twin::seen(&called), format!("root_output/{input}"));
            }
        }
    }

    /// The rows are written at `output`: a filter folding a param from the
    /// message, a projection, an ordering; and a count.
    #[tokio::test]
    async fn rows_are_written_at_output() {
        let read = query(json!({
            "source": "users",
            "filter": {">": [{"field": "age"}, {"param": "min"}]},
            "fields": ["id", "age"],
            "sort": [{"age": "asc"}]
        }));
        let mut with_params = read.clone();
        with_params["params"] = json!({"min": {"var": "data.threshold"}});
        let v4 = run(with_params, json!({"threshold": 18}), json!({})).await;
        twin::pinned(&v4, "rows_are_written_at_output/params");
        assert_eq!(
            v4.expect("read")["rows"],
            json!([{"id": "u2", "age": 20}, {"id": "u3", "age": 30}, {"id": "u4", "age": 40}])
        );

        // `params` that are not an object fold nothing, as 1.12's reader
        // read them, so the param is missing.
        for params in [json!([1]), json!({"var": "data.threshold"}), json!(null)] {
            let mut input = read.clone();
            input["params"] = params.clone();
            let v4 = run(input, json!({"threshold": 18}), json!({})).await;
            twin::pinned(&v4, format!("rows_are_written_at_output/params {params}"));
            assert!(v4.is_err(), "{params}");
        }

        let count = query(json!({"source": "users", "count": true}));
        let v4 = run(count, Value::Null, json!({})).await;
        twin::pinned(&v4, "rows_are_written_at_output/count");
        assert_eq!(v4.expect("counted")["rows"], json!({"count": 4}));
    }

    /// `numeric_as` and `binary_as` fold their holes and are read as 1.12
    /// read them.
    #[tokio::test]
    async fn numeric_as_and_binary_as_read_as_1_12_did() {
        let data = json!({"hex": "hex", "b64": "base64", "bad": "money"});
        for (numeric, binary, ok) in [
            (json!("string"), json!({"var": "data.hex"}), true),
            (json!(null), json!({"var": "data.b64"}), true),
            (json!({"var": "data.none"}), json!(null), true),
            (json!({"var": "data.bad"}), json!("hex"), false),
            (json!("number"), json!({"var": "data.bad"}), false),
        ] {
            let mut input = query(
                json!({"source": "users", "fields": ["id", "avatar"], "sort": [{"id": "asc"}]}),
            );
            input["numeric_as"] = numeric.clone();
            input["binary_as"] = binary.clone();
            let v4 = run(input, data.clone(), json!({})).await;
            twin::pinned(&v4, format!("numeric_as_binary_as/{numeric} {binary}"));
            assert_eq!(v4.is_ok(), ok, "{numeric} {binary}: {v4:?}");
        }
    }

    /// The gate, the connector, the schema the connector requires, the
    /// envelope and an output that names no path, each with 1.12's code and
    /// text.
    #[tokio::test]
    async fn refusals_are_1_12s() {
        let read = query(json!({"source": "users"}));
        let mut absent = read.clone();
        absent["connector"] = json!("absent");
        let mut no_schema = read.clone();
        no_schema.as_object_mut().expect("object").remove("schema");
        let mut computed = read.clone();
        computed["output"] = json!({"var": "data.n"});
        for (input, config) in [
            (read.clone(), json!({"operations": {"read": false}})),
            (absent, json!({})),
            (no_schema.clone(), json!({})),
            (read.clone(), json!({"dialect": {"require_schema": true}})),
            (query(json!({"source": "users", "count": 1})), json!({})),
            (query(json!({"source": "nowhere"})), json!({})),
            (computed, json!({})),
        ] {
            let key = format!("refusals/{input} {config}");
            let v4 = run(input.clone(), json!({"n": 1}), config).await;
            twin::pinned(&v4, key);
            assert!(v4.is_err(), "{input}");
        }
    }

    /// A connector of another kind is refused before anything is asked of
    /// it, as 1.12 refused it.
    #[tokio::test]
    async fn a_connector_of_another_kind_is_refused() {
        let connector = json!({"type": "cache", "backend": "memory"});
        let input = query(json!({"source": "users"}));
        let pool_cache = Arc::new(SqlPoolCache::new(8));
        let registry = twin::connectors(&[("db", connector)]).await;
        let v4 = twin::v4(NAME, function(pool_cache, registry), input, Value::Null).await;
        twin::pinned(&v4, "a_connector_of_another_kind_is_refused");
        assert!(v4.is_err());
    }

    /// `QueryBudget` within the run's deadline (E10), the one connection held
    /// so the query waits on the pool: a run deadline before the connector's
    /// `query_timeout_ms` ends the call `TimedOut`, as 1.12's channel timeout
    /// ended the run, writing nothing.
    #[tokio::test]
    async fn a_run_deadline_before_the_query_timeout_ends_the_call_timed_out() {
        for envelope in [
            json!({"source": "users"}),
            json!({"source": "users", "count": true}),
        ] {
            let db = twin::sqlite(json!({"query_timeout_ms": 30_000}), SEED).await;
            let _held = db.pool.acquire().await.expect("the one connection");
            let started = std::time::Instant::now();
            let called = testing::call(
                function(db.pool_cache.clone(), db.registry.clone()),
                query(envelope.clone()),
            )
            .exprs(kit::test_exprs())
            .name(NAME)
            .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
            .await;
            let error = called.outcome.expect_err("the run's deadline passed");
            assert_eq!(*error.kind(), ErrorKind::TimedOut, "{envelope}");
            assert_eq!(
                error.message(),
                "the run's deadline passed during `data_query`"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                started.elapsed()
            );
            assert!(called.writes.is_empty());
        }
    }

    /// A `query_timeout_ms` before the run's deadline is the budget's, with
    /// 1.12's code, text and retryability, for a row read and a count.
    #[tokio::test]
    async fn a_query_timeout_before_the_run_deadline_is_1_12s() {
        let config = json!({"query_timeout_ms": 150});
        for envelope in [
            json!({"source": "users"}),
            json!({"source": "users", "count": true}),
        ] {
            let input = query(envelope.clone());
            let db = twin::sqlite(config.clone(), SEED).await;
            let _held = db.pool.acquire().await.expect("the one connection");
            let called = testing::call(function(db.pool_cache.clone(), db.registry.clone()), input)
                .exprs(kit::test_exprs())
                .name(NAME)
                .deadline(dataflow_rs::run::Instant::now() + Duration::from_secs(60))
                .await;
            let v4 = twin::seen(&called);
            twin::pinned(
                &v4,
                format!("a_query_timeout_before_the_run_deadline/{envelope}"),
            );
            let failure = v4.expect_err("the query timed out");
            assert_eq!(failure.code, "TIMEOUT_ERROR");
            assert_eq!(
                failure.message,
                "Timeout error: data_query query timed out after 150ms"
            );
            assert!(failure.retryable);
        }
    }
}
