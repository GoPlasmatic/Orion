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
    ConnectorCall, QueryBudget, build_entity_registry, decode_failure, es_request, is_mongo,
    require_op_allowed, resolve_numeric_as, resolve_params, timed_query, to_connect_error,
    to_exec_error,
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
    /// How an arbitrary-precision decimal is rendered (#309). SQL backends
    /// only; Elasticsearch and MongoDB carry their own JSON types.
    numeric_as: crate::connector::sql_decode::NumericAs,
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
            numeric_as: resolve_numeric_as(input, call.name, ctx)?,
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
                run_es_search(&self.http_client, es, &eq).await?
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
                Value::Array(
                    docs.iter()
                        .filter_map(|d| serde_json::to_value(d).ok())
                        .collect(),
                )
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
                run_sql_with_includes(
                    &pool,
                    &plan,
                    dialect,
                    db.query_timeout_ms,
                    parsed.numeric_as,
                )
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

/// Execute an Elasticsearch search: POST the rendered body to
/// `{url}/{index}/_search` and return the `_source` of each hit as a JSON array.
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
                .map(|h| h.get("_source").cloned().unwrap_or(Value::Null))
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
    numeric: crate::connector::sql_decode::NumericAs,
) -> Result<Value, DataflowError> {
    let budget = QueryBudget::start(timeout_ms);
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    // `SqlxValues` implements `IntoArguments` for all three drivers as well as
    // `Any`, so the builder above needs no change — only the execute site
    // dispatches (#309).
    let mut parents: Vec<Value> = crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, _bind => {
            let rows = budget
                .run(NAME, sqlx::query_with(&sql, values).fetch_all(p))
                .await?;
            rows_to_json(&rows, numeric).map_err(|e| decode_failure(NAME, e))?
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
        if !keys.is_empty() {
            let (csql, cvalues) = query::backend::sql::build_include_select(inc, &keys, dialect);
            let children: Vec<Value> = crate::connector::pool_cache::dispatch_sql_pool!(
                pool, p, rows_to_json, _bind => {
                    let crows = budget
                        .run(NAME, sqlx::query_with(&csql, cvalues).fetch_all(p))
                        .await?;
                    rows_to_json(&crows, numeric).map_err(|e| decode_failure(NAME, e))?
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
        description: "Backend-neutral query envelope: source/filter/fields/sort/limit/skip/include. \
                      An include selection is {fields, sort, limit}; `sort` is required because \
                      the per-parent page is cut in the database.",
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
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
