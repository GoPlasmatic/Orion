use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use futures::TryStreamExt;
use mongodb::bson::Document;
use serde_json::Value;
use sqlx::any::AnyRow;

use super::connector_helpers::{
    ConnectorCall, apply_output, build_entity_registry, es_request, is_mongo, resolve_params,
    timed_query, to_connect_error, to_exec_error,
};
use super::db_read::rows_to_json;
use super::schema::{FieldKind, FieldSchema};
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::query::{self, GroupKey, SqlDialect};
use crate::storage::detect_backend;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "data_query";

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

#[async_trait]
impl AsyncFunctionHandler for DataQueryHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — `connector` and `query` are
        // both literal keys, so a task missing either must be told before the
        // message is consulted at all.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let query = input
            .get("query")
            .ok_or_else(|| DataflowError::Validation(format!("{NAME} requires 'query' field")))?;

        // Resolve `params` against the message context — the only point at
        // which the message touches the query. It produces concrete literals, not
        // SQL, which the pure translation path then folds into the filter.
        let params = resolve_params(input.get("params"), ctx);

        call.run(&self.registry, async {
            // Per-connector operation gates: reads can be disabled too
            // (e.g. a write-only audit sink).
            let connector_config = call.resolve(&self.registry, Some("read")).await?;

            // Optional inline schema (privileged config authored alongside the
            // query): renames, type hints, allowlist, and relation declarations.
            // F24: with no schema the registry now rejects rather than passing
            // every name through, and the connector's own guards apply on top.
            let registry =
                build_entity_registry(input.get("schema"), &connector_config, call.connector)?;

            let result = match connector_config.as_ref() {
                ConnectorConfig::Es(es) => {
                    // Elasticsearch: render a search body and POST it via HTTP.
                    let eq = query::translate_es(query, &params, &registry, &self.limits)?;
                    run_es_search(&self.http_client, es, &eq).await?
                }
                ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
                    // MongoDB: render a `find` and execute it via the Mongo pool.
                    let database = call.require_str(input, "database")?;
                    let mq = query::translate_mongo(query, &params, &registry, &self.limits)?;
                    let client = self
                        .mongo_pool_cache
                        .get_client(call.connector, db)
                        .await
                        .map_err(to_connect_error)?;
                    let coll = client
                        .database(database)
                        .collection::<Document>(&mq.collection);
                    // F11: DbConnectorConfig.query_timeout_ms never applied
                    // to Mongo — an unresponsive server hung the request for
                    // the channel timeout, which is itself optional.
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
                    // SQL: dialect from the connection-string scheme (the same
                    // source `AnyPool` uses), so the rendered SQL matches the pool.
                    let dialect: SqlDialect = detect_backend(&db.connection_string)
                        .map_err(to_exec_error)?
                        .into();
                    let plan = query::plan_sql(query, &params, &registry, dialect, &self.limits)?;
                    let pool = self
                        .pool_cache
                        .get_pool(call.connector, db)
                        .await
                        .map_err(to_connect_error)?;
                    run_sql_with_includes(&pool, &plan, dialect, db.query_timeout_ms).await?
                }
                _ => {
                    return Err(DataflowError::Validation(format!(
                        "Connector '{}' is not a db or es connector",
                        call.connector
                    )));
                }
            };

            apply_output(ctx, call.output, result);
            Ok(TaskOutcome::Success)
        })
        .await
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
async fn run_sql_with_includes(
    pool: &sqlx::AnyPool,
    plan: &query::SqlPlan,
    dialect: SqlDialect,
    timeout_ms: Option<u64>,
) -> Result<Value, DataflowError> {
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    let rows: Vec<AnyRow> = timed_query(
        timeout_ms,
        NAME,
        sqlx::query_with(&sql, values).fetch_all(pool),
    )
    .await?;
    let mut parents: Vec<Value> = rows_to_json(&rows)?;

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
            let crows: Vec<AnyRow> = timed_query(
                timeout_ms,
                NAME,
                sqlx::query_with(&csql, cvalues).fetch_all(pool),
            )
            .await?;
            let children = rows_to_json(&crows)?;
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
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "query",
        description: "Backend-neutral query envelope: source/filter/fields/sort/limit/skip/include. \
                      An include selection is {fields, sort, limit}; `sort` is required because \
                      the per-parent page is cut in the database.",
        kind: FieldKind::Object,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "database",
        description: "MongoDB database name. Optional here because the same task shape is \
                      valid against SQL and Elasticsearch, which need no database key; \
                      required — and checked at workflow activation — once the referenced \
                      connector is a MongoDB one (F52).",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "schema",
        description: "Inline entity schema (renames, type hints, allowlist, relations) enabling \
                      some/all/none and typed coercion. Undeclared entities and columns are \
                      rejected, so a query without one reaches nothing; pass \
                      {\"unmapped\": \"identity\"} for pre-1.0 pass-through.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into the filter's {\"param\": ..} nodes. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];
