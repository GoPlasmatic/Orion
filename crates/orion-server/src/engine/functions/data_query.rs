use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use futures::TryStreamExt;
use mongodb::bson::Document;
use serde_json::{Map, Value};

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, QueryBudget, QueryFailure, acquire_conn, build_entity_registry, decode_failure,
    encode_failure, es_request, is_mongo, require_op_allowed, resolve_params, resolve_row_format,
    timed_query, to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::engine::HandlerError;
use crate::query::{self, GroupKey, SqlDialect};
use crate::storage::detect_backend;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
const NAME: &str = <DataQueryHandler as ConnectorHandler>::NAME;

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

/// Executes a portable `data_query` — one backend-neutral filter + envelope that
/// renders to native SQL, a MongoDB `find`, or an Elasticsearch search — against a
/// SQL, Mongo, or ES connector. The translation lives in `src/query/`; this
/// handler wires it to the connector/pool machinery (ES runs over the HTTP client).
pub struct DataQueryHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    /// Page bounds (`default_limit` / `max_limit` / `max_skip`) from `[query]`.
    pub limits: crate::config::QueryConfig,
}

/// A portable query, read from the task with only its `params` folded against
/// the message.
///
/// `database` is optional here and required only on the Mongo path: the same
/// task shape is valid against SQL and Elasticsearch, which have a database in
/// the connector, so which answer is right is not known until the connector is
/// resolved.
pub struct DataQuery {
    query: Value,
    params: Map<String, Value>,
    schema: Option<Value>,
    /// How a decimal and a binary column render. SQL backends only;
    /// Elasticsearch and MongoDB carry their own JSON types.
    format: crate::connector::sql_decode::RowFormat,
    database: Option<String>,
}

#[async_trait]
impl ConnectorHandler for DataQueryHandler {
    const NAME: &'static str = "data_query";
    /// The portable dialect is the one pair that spans backends, so it names
    /// the union rather than a type: a `db` or an `es` connector, dispatched
    /// on the variant below.
    type Kind = crate::connector::DataBackend;
    type Input = TemplatedInput;
    type Parsed = DataQuery;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let query = input
            .get("query")
            .ok_or_else(|| {
                DataflowError::Validation(format!("{} requires 'query' field", call.name))
            })?
            .clone();

        Ok(DataQuery {
            query,
            // Resolved against the message — the only point at which the
            // message touches the query. It produces concrete literals, not
            // SQL, which the pure translation path then folds into the filter.
            params: resolve_params(input, <Self as ConnectorHandler>::NAME, ctx),
            format: resolve_row_format(input, call.name, ctx)?,
            // Optional inline schema (privileged config authored alongside the
            // query): renames, type hints, allowlist, relation declarations.
            schema: input.get("schema").cloned(),
            database: input
                .get("database")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &ConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // Per-connector operation gates: reads can be disabled too (e.g. a
        // write-only audit sink).
        if let Some(gates) = conn.operation_gates() {
            require_op_allowed(gates, "read", connector)?;
        }
        Ok(())
    }

    async fn run(
        &self,
        parsed: Self::Parsed,
        conn: &ConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        // F24: with no schema the registry rejects rather than passing every
        // name through, and the connector's own guards apply on top.
        let registry = build_entity_registry(parsed.schema.as_ref(), conn, call.connector)?;
        let query = &parsed.query;
        let params = &parsed.params;

        let result = match conn {
            ConnectorConfig::Es(es) => {
                // Elasticsearch: render a search body and POST it via HTTP.
                let eq = query::translate_es(query, params, &registry, &self.limits)
                    .map_err(DataflowError::from)?;
                if eq.count {
                    run_es_count(&self.http_client, es, &eq).await?
                } else {
                    run_es_search(&self.http_client, es, &eq).await?
                }
            }
            ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
                // MongoDB: render a `find` and execute it via the Mongo pool.
                // The database is required here and nowhere else — a MongoDB
                // connection string carries no default one.
                let database = parsed.database.as_deref().ok_or_else(|| {
                    DataflowError::Validation(format!("{} requires 'database' field", call.name))
                })?;
                let mq = query::translate_mongo(query, params, &registry, &self.limits)
                    .map_err(DataflowError::from)?;
                let client = self
                    .mongo_pool_cache
                    .get_client(call.connector, db)
                    .await
                    .map_err(to_connect_error)?;
                let coll = client
                    .database(database)
                    .collection::<Document>(&mq.collection);
                // F11: DbConnectorConfig.query_timeout_ms never applied to
                // Mongo — an unresponsive server hung the request for the
                // channel timeout, which is itself optional.
                if mq.count {
                    let n = timed_query(db.query_timeout_ms, call.name, async {
                        coll.count_documents(mq.filter)
                            .await
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                    return Ok(count_result(n).into());
                }
                let docs: Vec<Document> = timed_query(db.query_timeout_ms, call.name, async {
                    let mut find = coll.find(mq.filter);
                    if let Some(p) = mq.projection {
                        find = find.projection(p);
                    }
                    if let Some(s) = mq.sort {
                        find = find.sort(s);
                    }
                    if let Some(sk) = mq.skip {
                        find = find.skip(sk);
                    }
                    find = find.limit(mq.limit as i64);
                    let cursor = find.await.map_err(|e| e.to_string())?;
                    cursor.try_collect().await.map_err(|e| e.to_string())
                })
                .await?;
                // Shared with `mongo_read`/`mongo_aggregate`, so an
                // unserializable document is the same named error on every
                // MongoDB read path rather than a silently shorter array here.
                super::mongo_common::docs_to_json(&docs, NAME)?
            }
            ConnectorConfig::Db(db) => {
                // SQL: dialect from the connection-string scheme, the same
                // source the pool is built from, so the rendered SQL always
                // matches the driver that runs it.
                let dialect: SqlDialect = detect_backend(&db.connection_string)
                    .map_err(to_exec_error)?
                    .into();
                let plan = query::plan_sql(query, params, &registry, dialect, &self.limits)
                    .map_err(DataflowError::from)?;
                let pool = self
                    .pool_cache
                    .get_pool(call.connector, db)
                    .await
                    .map_err(to_connect_error)?;
                if plan.count {
                    return Ok(run_sql_count(&pool, &plan, dialect, db.query_timeout_ms)
                        .await?
                        .into());
                }
                run_sql_with_includes(&pool, &plan, dialect, db.query_timeout_ms, parsed.format)
                    .await?
            }
            // `DataBackend` admits `db` and `es` and nothing else, and it
            // produced the "is not a db or es connector" refusal — the one this
            // handler used to write itself, here, after resolving.
            other => unreachable!(
                "DataBackend admitted a '{}' connector",
                other.connector_type()
            ),
        };

        Ok(result.into())
    }
}

/// The shape every backend answers a `"count": true` envelope with.
///
/// One object, one key, whatever the backend counted with — the point of the
/// portable dialect is that the caller cannot tell which one ran.
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
) -> Result<Value, DataflowError> {
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    // Two legs now — acquire, then the statement — so they share one budget
    // rather than each getting the connector's whole `query_timeout_ms`.
    let budget = QueryBudget::start(timeout_ms);
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
                crate::connector::sql_encode::Bound::Typed(args) => sqlx::query_with(&sql, args),
                crate::connector::sql_encode::Bound::Fallback { cache } => {
                    sqlx::query_with(&sql, fallback)
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
            DataflowError::function_execution(
                format!("{NAME}: the count query returned no count"),
                None,
            )
        })?;
    Ok(count_result(n))
}

/// Execute an Elasticsearch count: POST the rendered query to
/// `{url}/{index}/_count` and read the `count` it answers with.
async fn run_es_count(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    eq: &query::backend::es::EsQuery,
) -> Result<Value, DataflowError> {
    let url = format!("{}/{}/_count", es.url.trim_end_matches('/'), eq.index);
    let req = es_request(client, es, reqwest::Method::POST, &url)
        .await?
        .json(&eq.body);

    let (status, body) = super::connector_helpers::send_es(req, es.max_response_size).await?;
    if !status.is_success() {
        return Err(DataflowError::function_execution(
            format!("Elasticsearch count failed ({status}): {body}"),
            None,
        ));
    }
    let n = body.get("count").and_then(Value::as_u64).ok_or_else(|| {
        DataflowError::function_execution(
            format!("Elasticsearch count returned no count: {body}"),
            None,
        )
    })?;
    Ok(count_result(n))
}

/// Execute an Elasticsearch search: POST the rendered body to
/// `{url}/{index}/_search` and return the `_source` of each hit as a JSON array.
///
/// When the projection named the document key, `_id` is lifted out of the hit
/// and into the returned document. It has to be: ES keeps `_id` *outside*
/// `_source`, so a schema declaring the `id` → `_id` rename — the spelling the
/// write path requires — asked for `"_source": ["_id"]` and got `{}` back for
/// every hit. The write path already lifts a physical `_id` the other way, into
/// the bulk action; this is the read half of the same rename.
async fn run_es_search(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    eq: &query::backend::es::EsQuery,
) -> Result<Value, DataflowError> {
    let url = format!("{}/{}/_search", es.url.trim_end_matches('/'), eq.index);
    let req = es_request(client, es, reqwest::Method::POST, &url)
        .await?
        .json(&eq.body);

    let (status, body) = super::connector_helpers::send_es(req, es.max_response_size).await?;
    if !status.is_success() {
        return Err(DataflowError::function_execution(
            format!("Elasticsearch search failed ({status}): {body}"),
            None,
        ));
    }

    let docs: Vec<Value> = body
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array())
        .map(|hits| {
            hits.iter()
                .map(|h| {
                    let mut source = h.get("_source").cloned().unwrap_or(Value::Null);
                    if eq.include_id
                        && let Some(id) = h.get(query::backend::es::ES_DOCUMENT_KEY)
                        && let Value::Object(map) = &mut source
                    {
                        map.insert(query::backend::es::ES_DOCUMENT_KEY.to_string(), id.clone());
                    }
                    source
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Value::Array(docs))
}

/// Execute the main SQL query, then hydrate each `include` with a per-relation
/// child query (`WHERE fk IN (parent keys)`), grouping children back to their
/// parents in memory. One extra query per include relation; no per-dialect JSON
/// functions needed.
///
/// The per-parent page is cut **in the child query** (`ROW_NUMBER() OVER
/// (PARTITION BY fk ORDER BY …)`, see
/// [`query::backend::sql::build_include_select`]), not here: this used to fetch
/// every child of every parent on the page and truncate afterwards, so a
/// thousand parents with ten thousand children each materialised ten million
/// rows to return five apiece — in an order nothing defined (F27).
///
/// Grouping is by [`query::GroupKey`], not by the key's JSON text, so a parent
/// key and a child foreign key that the driver rendered differently (`"7"` vs
/// `7`) still join (W14).
///
/// Every round trip — the main query and one child query per include relation —
/// shares a single [`QueryBudget`]. Each used to start its own `timed_query`,
/// which is a fresh budget, so a query with three includes could run for four
/// times the `query_timeout_ms` its connector was configured with. That is the
/// hazard `QueryBudget` exists to close on the write path, and the read path
/// with includes is the other place a logical operation spans round trips.
async fn run_sql_with_includes(
    pool: &crate::connector::pool_cache::SqlPool,
    plan: &query::SqlPlan,
    dialect: SqlDialect,
    timeout_ms: Option<u64>,
    format: crate::connector::sql_decode::RowFormat,
) -> Result<Value, DataflowError> {
    let budget = QueryBudget::start(timeout_ms);
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
                crate::connector::sql_encode::Bound::Typed(args) => sqlx::query_with(&sql, args),
                crate::connector::sql_encode::Bound::Fallback { cache } => {
                    sqlx::query_with(&sql, fallback)
                        .persistent(cache)
                }
            };
            let rows = budget.run(NAME, q.fetch_all(&mut *conn)).await?;
            rows_to_json(&rows, format).map_err(|e| decode_failure(NAME, e))?
        }
    );

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
                            sqlx::query_with(&csql, args)
                        }
                        crate::connector::sql_encode::Bound::Fallback { cache } => {
                            sqlx::query_with(&csql, fallback)
                                .persistent(cache)
                        }
                    };
                    let crows = budget.run(NAME, q.fetch_all(&mut *conn)).await?;
                    rows_to_json(&crows, format).map_err(|e| decode_failure(NAME, e))?
                }
            );
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

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DATA_QUERY_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the db (SQL/MongoDB) or es (Elasticsearch) connector to query.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "Backend-neutral query envelope: \
                      source/filter/fields/sort/limit/skip/after/include/count. \
                      An include selection is {fields, sort, limit}; `sort` is required because \
                      the per-parent page is cut in the database. \"after\" is a keyset cursor — \
                      the previous page's last row, one value per sort key — which pages without \
                      an offset and so is not bounded by query.max_skip. \"count\": true answers \
                      {\"count\": n} instead of rows, and refuses the keys that shape a row set.",
        kind: FieldKind::Object,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into the filter's {\"param\": ..} nodes. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "schema",
        description: "Inline entity schema (renames, type hints, allowlist, relations) enabling \
                      some/all/none and typed coercion. Undeclared entities and columns are \
                      rejected, so a query without one reaches nothing; pass \
                      {\"unmapped\": \"identity\"} for pre-1.0 pass-through.",
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
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
